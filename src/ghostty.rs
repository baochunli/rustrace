use crate::{CommandExecution, DEFAULT_CAPTURE_LIMIT_BYTES, SystemCommandRunner};
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub const SETUP_KEYS: [&str; 12] = [
    "super+arrow_up",
    "super+arrow_down",
    "super+arrow_left",
    "super+arrow_right",
    "super+backspace",
    "alt+arrow_left",
    "alt+arrow_right",
    "super+f",
    "super+z",
    "super+k",
    "super+home",
    "super+end",
];

const DEFAULT_ACTIONS: [&str; 12] = [
    "jump_to_prompt:-1",
    "jump_to_prompt:1",
    r"text:\x01",
    r"text:\x05",
    r"text:\x15",
    "esc:b",
    "esc:f",
    "start_search",
    "undo",
    "clear_screen",
    "scroll_to_top",
    "scroll_to_bottom",
];

pub const KEYS_MARKER: &str = "# Rustrace Ghostty key setup";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhosttyBindingStatus {
    Owned,
    Rewritten,
    Passed,
}

impl GhosttyBindingStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Rewritten => "rewritten",
            Self::Passed => "passed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GhosttyKeyBindings {
    statuses: [GhosttyBindingStatus; SETUP_KEYS.len()],
    exact_translations: [bool; SETUP_KEYS.len()],
    probe_available: bool,
}

impl GhosttyKeyBindings {
    pub const fn passed() -> Self {
        Self {
            statuses: [GhosttyBindingStatus::Passed; SETUP_KEYS.len()],
            exact_translations: [false; SETUP_KEYS.len()],
            probe_available: true,
        }
    }

    pub fn defaults() -> Self {
        Self {
            statuses: DEFAULT_ACTIONS.map(classify_action),
            exact_translations: std::array::from_fn(|index| {
                is_exact_translation(SETUP_KEYS[index], DEFAULT_ACTIONS[index])
            }),
            probe_available: false,
        }
    }

    pub fn from_list_output(output: &str) -> Self {
        parse_list_output(output).bindings
    }

    pub fn status(&self, key: &str) -> Option<GhosttyBindingStatus> {
        SETUP_KEYS
            .iter()
            .position(|candidate| *candidate == key)
            .map(|index| self.statuses[index])
    }

    pub fn command_hint_available(&self, key: &str) -> bool {
        self.probe_available
            && SETUP_KEYS
                .iter()
                .position(|candidate| *candidate == key)
                .is_some_and(|index| {
                    self.statuses[index] == GhosttyBindingStatus::Passed
                        || self.exact_translations[index]
                })
    }

    pub(crate) fn observed_exact_translation(&self, key: &str) -> bool {
        self.probe_available
            && SETUP_KEYS
                .iter()
                .position(|candidate| *candidate == key)
                .is_some_and(|index| self.exact_translations[index])
    }
}

impl Default for GhosttyKeyBindings {
    fn default() -> Self {
        Self::defaults()
    }
}

#[derive(Debug)]
pub(crate) struct GhosttyInspection {
    pub bindings: GhosttyKeyBindings,
    pub actions: [Option<String>; SETUP_KEYS.len()],
    pub cli_available: bool,
}

pub(crate) fn applies_to_current_terminal() -> bool {
    cfg!(target_os = "macos")
        && env::var("TERM_PROGRAM").is_ok_and(|terminal| terminal.eq_ignore_ascii_case("ghostty"))
}

pub(crate) fn inspect_current() -> GhosttyInspection {
    let Some(binary) = ghostty_binary() else {
        return default_inspection();
    };
    let runner =
        SystemCommandRunner::with_limits(crate::DEFAULT_PROBE_TIMEOUT, DEFAULT_CAPTURE_LIMIT_BYTES);
    match runner.run_command(Command::new(binary).arg("+list-keybinds")) {
        CommandExecution::Succeeded { stdout, .. } => parse_list_output(&stdout),
        CommandExecution::NotFound
        | CommandExecution::Failed { .. }
        | CommandExecution::TimedOut { .. } => default_inspection(),
    }
}

pub(crate) fn startup_key_bindings() -> GhosttyKeyBindings {
    if applies_to_current_terminal() {
        inspect_current().bindings
    } else {
        GhosttyKeyBindings::passed()
    }
}

pub fn ghostty_keys_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(PathBuf::from).map(|path| path.join(".config")))
        .map(|path| path.join("ghostty/rustrace-keys"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteOutcome {
    Written,
    UpToDate,
}

pub(crate) fn write_keys_from_environment() -> Result<(PathBuf, WriteOutcome), String> {
    let path = ghostty_keys_path(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
    .ok_or_else(|| "XDG_CONFIG_HOME and HOME are unavailable".to_owned())?;
    let contents = keys_file_contents();

    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "refusing to overwrite {}; it is a symbolic link",
                path.display()
            ));
        }
        Ok(_) => {
            let existing = fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if existing == contents {
                return Ok((path, WriteOutcome::UpToDate));
            }
            if existing.lines().next() != Some(KEYS_MARKER) {
                return Err(format!(
                    "refusing to overwrite {}; it does not have the Rustrace marker",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    }

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    fs::write(&path, contents)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok((path, WriteOutcome::Written))
}

fn ghostty_binary() -> Option<PathBuf> {
    env::var_os("GHOSTTY_BIN_DIR")
        .filter(|directory| !directory.is_empty())
        .map(PathBuf::from)
        .map(|directory| directory.join("ghostty"))
        .filter(|path| path.is_file())
        .or_else(|| {
            let path = Path::new("/Applications/Ghostty.app/Contents/MacOS/ghostty");
            path.is_file().then(|| path.to_owned())
        })
}

fn default_inspection() -> GhosttyInspection {
    GhosttyInspection {
        bindings: GhosttyKeyBindings::defaults(),
        actions: DEFAULT_ACTIONS.map(|action| Some(action.to_owned())),
        cli_available: false,
    }
}

fn parse_list_output(output: &str) -> GhosttyInspection {
    let probe_available = output
        .lines()
        .any(|line| binding_definition(line).is_some());
    if !probe_available {
        return default_inspection();
    }
    let actions = std::array::from_fn(|index| action_for_key(output, SETUP_KEYS[index]));
    let statuses = std::array::from_fn(|index| {
        actions[index]
            .as_deref()
            .map_or(GhosttyBindingStatus::Passed, classify_action)
    });
    let exact_translations = std::array::from_fn(|index| {
        actions[index]
            .as_deref()
            .is_some_and(|action| is_exact_translation(SETUP_KEYS[index], action))
    });
    GhosttyInspection {
        bindings: GhosttyKeyBindings {
            statuses,
            exact_translations,
            probe_available,
        },
        actions,
        cli_available: true,
    }
}

fn action_for_key(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (candidate, action) = binding_definition(line)?;
        (candidate.rsplit(':').next() == Some(key) && action != "unbind").then(|| action.to_owned())
    })
}

fn binding_definition(line: &str) -> Option<(&str, &str)> {
    line.trim().strip_prefix("keybind = ")?.split_once('=')
}

fn classify_action(action: &str) -> GhosttyBindingStatus {
    if action == "unbind" {
        GhosttyBindingStatus::Passed
    } else if action.starts_with("text:") || action.starts_with("esc:") {
        GhosttyBindingStatus::Rewritten
    } else {
        GhosttyBindingStatus::Owned
    }
}

fn is_exact_translation(key: &str, action: &str) -> bool {
    match key {
        "super+arrow_left" => serialized_text_byte(action) == Some(0x01),
        "super+arrow_right" => serialized_text_byte(action) == Some(0x05),
        "super+backspace" => serialized_text_byte(action) == Some(0x15),
        "super+k" => serialized_text_byte(action) == Some(0x0b),
        "alt+arrow_left" => action == "esc:b",
        "alt+arrow_right" => action == "esc:f",
        _ => false,
    }
}

fn serialized_text_byte(action: &str) -> Option<u8> {
    let payload = action.strip_prefix("text:")?;
    if let [byte] = payload.as_bytes() {
        return Some(*byte);
    }

    let escaped = payload.trim_start_matches('\\');
    if escaped.len() == payload.len() {
        return None;
    }
    let hex = escaped
        .strip_prefix('x')
        .or_else(|| escaped.strip_prefix('X'))?;
    if hex.len() != 2 {
        return None;
    }
    u8::from_str_radix(hex, 16).ok()
}

fn keys_file_contents() -> String {
    let mut contents = String::from(KEYS_MARKER);
    contents.push('\n');
    for key in SETUP_KEYS {
        contents.push_str("keybind = ");
        contents.push_str(key);
        contents.push_str("=unbind\n");
    }
    contents
}

#[cfg(test)]
mod tests {
    use super::{GhosttyBindingStatus, SETUP_KEYS, default_inspection, parse_list_output};

    #[test]
    fn empty_and_unparseable_probe_output_use_the_no_inference_model() {
        let expected = default_inspection();

        for output in ["", "Ghostty emitted no key binding report\n"] {
            let inspection = parse_list_output(output);

            assert!(
                !inspection.cli_available,
                "output was treated as usable: {output:?}"
            );
            assert_eq!(inspection.actions, expected.actions, "{output:?}");
            for key in SETUP_KEYS {
                assert_eq!(
                    inspection.bindings.status(key),
                    expected.bindings.status(key),
                    "{output:?}: {key}"
                );
                assert_ne!(
                    inspection.bindings.status(key),
                    Some(GhosttyBindingStatus::Passed),
                    "{output:?}: absent {key} was inferred to pass"
                );
                assert!(
                    !inspection.bindings.command_hint_available(key),
                    "{output:?}: {key}"
                );
                assert!(
                    !inspection.bindings.observed_exact_translation(key),
                    "{output:?}: {key}"
                );
            }
        }
    }
}
