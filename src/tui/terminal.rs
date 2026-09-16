use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

use super::theme::{EffectiveTheme, ThemeConfig, resolve_theme};

const OSC_11_QUERY: &[u8] = b"\x1b]11;?\x1b\\";
const OSC_11_TIMEOUT: Duration = Duration::from_millis(150);
const OSC_11_RESPONSE_LIMIT: usize = 256;

/// Terminal side effects required by [`TerminalSession`].
///
/// Injecting this boundary keeps lifecycle tests isolated from process-global
/// raw mode and the user's real terminal.
pub trait TerminalOperations {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    fn enable_mouse_capture(&mut self) -> io::Result<()>;
    fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
        Ok(false)
    }
    fn push_keyboard_enhancement(&mut self, _flags: KeyboardEnhancementFlags) -> io::Result<()> {
        Ok(())
    }
    fn pop_keyboard_enhancement(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn query_background_color(&mut self, _timeout: Duration) -> io::Result<Option<[u8; 3]>> {
        Ok(None)
    }
    fn write_system_clipboard(&mut self, _text: &str) -> io::Result<()> {
        Ok(())
    }
    fn disable_mouse_capture(&mut self) -> io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrosstermTerminalOperations;

impl TerminalOperations for CrosstermTerminalOperations {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableBracketedPaste)
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }

    fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    fn push_keyboard_enhancement(&mut self, flags: KeyboardEnhancementFlags) -> io::Result<()> {
        execute!(io::stdout(), PushKeyboardEnhancementFlags(flags))
    }

    fn pop_keyboard_enhancement(&mut self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
    }

    fn query_background_color(&mut self, timeout: Duration) -> io::Result<Option<[u8; 3]>> {
        query_background_color(timeout)
    }

    fn write_system_clipboard(&mut self, text: &str) -> io::Result<()> {
        let sequence = osc52_clipboard_sequence(text);
        let mut stdout = io::stdout().lock();
        stdout.write_all(&sequence)?;
        stdout.flush()
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableBracketedPaste)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

/// Owns terminal setup and restores it when dropped.
///
/// Cleanup is armed as soon as raw mode succeeds. This means a later setup
/// failure, normal return, or unwinding panic all restore the screen and raw
/// mode. Aborting panics cannot run Rust destructors and are outside this
/// boundary's guarantee.
#[must_use = "dropping the terminal session immediately restores the terminal"]
pub struct TerminalSession<O>
where
    O: TerminalOperations,
{
    operations: O,
    cleanup_armed: bool,
    keyboard_enhancement_supported: bool,
    keyboard_enhancement_pushed: bool,
}

impl<O> TerminalSession<O>
where
    O: TerminalOperations,
{
    pub fn enter(mut operations: O) -> io::Result<Self> {
        operations.enable_raw_mode()?;
        let mut session = Self {
            operations,
            cleanup_armed: true,
            keyboard_enhancement_supported: false,
            keyboard_enhancement_pushed: false,
        };
        session.operations.enter_alternate_screen()?;
        session.operations.enable_bracketed_paste()?;
        if session.operations.enable_mouse_capture().is_err() {
            eprintln!("Rustrace mouse capture unavailable; keyboard input remains available");
        }
        session.keyboard_enhancement_supported = session
            .operations
            .supports_keyboard_enhancement()
            .unwrap_or(false);
        if session.keyboard_enhancement_supported {
            session.operations.push_keyboard_enhancement(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
            )?;
            session.keyboard_enhancement_pushed = true;
        }
        Ok(session)
    }

    pub const fn keyboard_enhancement_supported(&self) -> bool {
        self.keyboard_enhancement_supported
    }

    pub const fn keyboard_enhancement_active(&self) -> bool {
        self.keyboard_enhancement_pushed
    }

    pub(crate) fn operations_mut(&mut self) -> &mut O {
        &mut self.operations
    }

    pub fn resolve_theme(
        &mut self,
        config: &ThemeConfig,
        colorterm: Option<&str>,
    ) -> EffectiveTheme {
        let background = if config.auto_switch {
            self.operations
                .query_background_color(OSC_11_TIMEOUT)
                .unwrap_or(None)
        } else {
            None
        };
        resolve_theme(config, colorterm, background)
    }

    fn restore(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        self.cleanup_armed = false;
        if self.keyboard_enhancement_pushed {
            let _ = self.operations.pop_keyboard_enhancement();
        }
        let _ = self.operations.disable_mouse_capture();
        let _ = self.operations.disable_bracketed_paste();
        let _ = self.operations.show_cursor();
        let _ = self.operations.leave_alternate_screen();
        let _ = self.operations.disable_raw_mode();
    }
}

pub(crate) fn osc52_clipboard_sequence(text: &str) -> Vec<u8> {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = text.as_bytes();
    let encoded_len = input.len().div_ceil(3) * 4;
    let mut sequence = Vec::with_capacity(8 + encoded_len);
    sequence.extend_from_slice(b"\x1b]52;c;");
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        sequence.push(BASE64[usize::from(first >> 2)]);
        sequence.push(BASE64[usize::from(((first & 0x03) << 4) | (second >> 4))]);
        sequence.push(if chunk.len() > 1 {
            BASE64[usize::from(((second & 0x0f) << 2) | (third >> 6))]
        } else {
            b'='
        });
        sequence.push(if chunk.len() > 2 {
            BASE64[usize::from(third & 0x3f)]
        } else {
            b'='
        });
    }
    sequence.push(0x07);
    sequence
}

#[cfg(unix)]
fn query_background_color(timeout: Duration) -> io::Result<Option<[u8; 3]>> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(OSC_11_QUERY)?;
    stdout.flush()?;

    let started = Instant::now();
    let mut response = Vec::with_capacity(64);
    while response.len() < OSC_11_RESPONSE_LIMIT {
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            break;
        };
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized pollfd for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == 0 {
            break;
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut chunk = [0_u8; 64];
        let available = (OSC_11_RESPONSE_LIMIT - response.len()).min(chunk.len());
        // SAFETY: `chunk` is writable for `available` bytes and stdin remains open.
        let read = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), available) };
        if read <= 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read as usize]);
        if let Some(color) = parse_osc_11_response(&response) {
            return Ok(Some(color));
        }
    }
    Ok(None)
}

#[cfg(not(unix))]
fn query_background_color(_timeout: Duration) -> io::Result<Option<[u8; 3]>> {
    Ok(None)
}

fn parse_osc_11_response(response: &[u8]) -> Option<[u8; 3]> {
    let start = response
        .windows(5)
        .position(|window| window == b"\x1b]11;")?
        + 5;
    let remaining = &response[start..];
    let end = remaining
        .iter()
        .position(|byte| *byte == 0x07)
        .or_else(|| remaining.windows(2).position(|window| window == b"\x1b\\"))?;
    let payload = std::str::from_utf8(&remaining[..end]).ok()?;
    let components = payload.strip_prefix("rgb:")?;
    let mut components = components.split('/');
    let color = [
        parse_osc_component(components.next()?)?,
        parse_osc_component(components.next()?)?,
        parse_osc_component(components.next()?)?,
    ];
    (components.next().is_none()).then_some(color)
}

fn parse_osc_component(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let maximum = (1_u32 << (component.len() * 4)) - 1;
    Some(((value * 255 + maximum / 2) / maximum) as u8)
}

impl<O> Drop for TerminalSession<O>
where
    O: TerminalOperations,
{
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::parse_osc_11_response;

    #[test]
    fn osc_11_parser_accepts_bel_and_st_terminated_rgb_replies() {
        assert_eq!(
            parse_osc_11_response(b"\x1b]11;rgb:ffff/8080/0000\x07"),
            Some([255, 128, 0])
        );
        assert_eq!(
            parse_osc_11_response(b"prefix\x1b]11;rgb:12/34/56\x1b\\suffix"),
            Some([0x12, 0x34, 0x56])
        );
        assert_eq!(parse_osc_11_response(b"\x1b]10;rgb:ff/ff/ff\x07"), None);
    }
}
