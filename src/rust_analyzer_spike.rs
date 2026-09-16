use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use rustrace_editor::position::Utf16Position as Position;
use rustrace_editor::position::{self, ByteOffset, LSP_POSITION_ENCODING};
use serde_json::{Value, json};

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const INCOMING_QUEUE_CAPACITY: usize = 16;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_CONFIRM_TIMEOUT: Duration = Duration::from_millis(100);
const STDERR_SYNC_TIMEOUT: Duration = Duration::from_millis(500);

const SOURCE_V1: &str = r#"struct Record {
    naïve: i32,
    名称: &'static str,
}

fn main() {
    let 東京 = Record { naïve: , 名称: "東京" };
    let _label = "naïve 名称 東京";
}
"#;

const SOURCE_V2: &str = r#"struct Record {
    naïve: i32,
    名称: &'static str,
}

fn main() {
    let 東京 = Record { naïve: 1, 名称: "東京" };
    let _label = "naïve 名称 東京"; 東京.
}
"#;

pub fn utf16_position(text: &str, byte_offset: usize) -> Result<Position, String> {
    position::utf16_position(text, ByteOffset(byte_offset as u64))
        .map_err(|error| error.to_string())
}

pub fn write_frame(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(invalid_data)?;
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "outgoing LSP message is {} bytes; limit is {MAX_MESSAGE_BYTES}",
                body.len()
            ),
        ));
    }
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

pub fn read_frame(reader: &mut impl Read, max_message_bytes: usize) -> io::Result<Option<Value>> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];

    while !header.ends_with(b"\r\n\r\n") {
        match reader.read(&mut byte) {
            Ok(0) if header.is_empty() => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "rust-analyzer closed stdout in the middle of an LSP header",
                ));
            }
            Ok(_) => header.push(byte[0]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if header.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("LSP header exceeds {MAX_HEADER_BYTES} bytes"),
            ));
        }
    }

    let header_text = std::str::from_utf8(&header[..header.len() - 4]).map_err(invalid_data)?;
    let mut content_length = None;
    for line in header_text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("malformed LSP header line: {line:?}"),
            ));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "duplicate Content-Length header",
                ));
            }
            content_length = Some(value.trim().parse::<usize>().map_err(invalid_data)?);
        }
    }

    let content_length = content_length
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "missing Content-Length header"))?;
    if content_length > max_message_bytes {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("LSP message is {content_length} bytes; limit is {max_message_bytes}"),
        ));
    }

    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(invalid_data)
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentSnapshot {
    BeforeOpen,
    Version(i32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Freshness {
    Current(DocumentSnapshot),
    Stale {
        observed: DocumentSnapshot,
        current: DocumentSnapshot,
    },
    Unversioned {
        current: DocumentSnapshot,
    },
}

#[derive(Debug)]
pub struct VersionState {
    current: DocumentSnapshot,
    pending: BTreeMap<i64, DocumentSnapshot>,
}

impl Default for VersionState {
    fn default() -> Self {
        Self::new()
    }
}

impl VersionState {
    pub fn new() -> Self {
        Self {
            current: DocumentSnapshot::BeforeOpen,
            pending: BTreeMap::new(),
        }
    }

    pub fn open(&mut self, version: i32) -> Result<(), String> {
        if self.current != DocumentSnapshot::BeforeOpen || version < 0 {
            return Err(format!(
                "cannot open document at version {version} from {:?}",
                self.current
            ));
        }
        self.current = DocumentSnapshot::Version(version);
        Ok(())
    }

    pub fn change(&mut self, version: i32) -> Result<(), String> {
        let DocumentSnapshot::Version(current) = self.current else {
            return Err("cannot change a document before didOpen".to_owned());
        };
        if version <= current {
            return Err(format!(
                "didChange version {version} must be greater than current version {current}"
            ));
        }
        self.current = DocumentSnapshot::Version(version);
        Ok(())
    }

    pub fn register_request(&mut self, id: i64, method: &str) -> Result<(), String> {
        if method.is_empty() {
            return Err("request method cannot be empty".to_owned());
        }
        if self.pending.insert(id, self.current).is_some() {
            return Err(format!("duplicate JSON-RPC request id {id}"));
        }
        Ok(())
    }

    pub fn finish_request(&mut self, id: i64) -> Result<Freshness, String> {
        let observed = self
            .pending
            .remove(&id)
            .ok_or_else(|| format!("response has unknown JSON-RPC id {id}"))?;
        Ok(classify_snapshot(observed, self.current))
    }

    pub fn classify_diagnostics(&self, version: Option<i32>) -> Freshness {
        let Some(version) = version else {
            return Freshness::Unversioned {
                current: self.current,
            };
        };
        classify_snapshot(DocumentSnapshot::Version(version), self.current)
    }

    fn current(&self) -> DocumentSnapshot {
        self.current
    }
}

fn classify_snapshot(observed: DocumentSnapshot, current: DocumentSnapshot) -> Freshness {
    if observed == current {
        Freshness::Current(current)
    } else {
        Freshness::Stale { observed, current }
    }
}

pub trait Transport {
    fn send(&mut self, message: &Value) -> Result<(), String>;
    fn receive(&mut self, timeout: Duration) -> Result<Value, String>;
}

pub fn run_protocol(
    transport: &mut impl Transport,
    root_uri: &str,
    document_uri: &str,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut versions = VersionState::new();

    send_request(
        transport,
        &mut versions,
        1,
        "initialize",
        json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "rustrace-rust-analyzer-spike", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "rustrace-spike"}],
            "capabilities": {
                "general": {"positionEncodings": [LSP_POSITION_ENCODING]},
                "textDocument": {
                    "publishDiagnostics": {"versionSupport": true},
                    "completion": {"completionItem": {}}
                },
                "workspace": {"workspaceFolders": true}
            },
            "trace": "off"
        }),
    )?;
    let initialize =
        wait_for_response(transport, &mut versions, 1, document_uri, root_uri, output)?;
    let position_encoding = initialize
        .pointer("/capabilities/positionEncoding")
        .and_then(Value::as_str)
        .unwrap_or(LSP_POSITION_ENCODING);
    if position_encoding != LSP_POSITION_ENCODING {
        return Err(format!(
            "rust-analyzer selected unsupported position encoding {position_encoding:?}; this spike requires UTF-16"
        ));
    }

    send_notification(transport, "initialized", json!({}))?;
    versions.open(1)?;
    send_notification(
        transport,
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": document_uri,
                "languageId": "rust",
                "version": 1,
                "text": SOURCE_V1
            }
        }),
    )?;
    wait_for_diagnostics(transport, &mut versions, 1, document_uri, root_uri, output)?;

    versions.change(2)?;
    send_notification(
        transport,
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": document_uri, "version": 2},
            "contentChanges": [{"text": SOURCE_V2}]
        }),
    )?;
    wait_for_diagnostics(transport, &mut versions, 2, document_uri, root_uri, output)?;

    let marker = "東京.";
    let byte_offset = SOURCE_V2
        .rfind(marker)
        .map(|offset| offset + marker.len())
        .ok_or_else(|| "completion marker is absent from spike source".to_owned())?;
    let position = utf16_position(SOURCE_V2, byte_offset)?;
    let byte_column = SOURCE_V2[..byte_offset]
        .rsplit_once('\n')
        .map_or(byte_offset, |(_, line)| line.len());
    writeln!(
        output,
        "UTF-16 completion position: {}:{} (UTF-8 byte column {byte_column})",
        position.line, position.character
    )
    .map_err(|error| error.to_string())?;

    send_request(
        transport,
        &mut versions,
        2,
        "textDocument/completion",
        json!({
            "textDocument": {"uri": document_uri},
            "position": {"line": position.line, "character": position.character},
            "context": {"triggerKind": 1}
        }),
    )?;
    let completion =
        wait_for_response(transport, &mut versions, 2, document_uri, root_uri, output)?;
    print_completion_summaries(&completion, output)?;

    send_request(transport, &mut versions, 3, "shutdown", Value::Null)?;
    wait_for_response(transport, &mut versions, 3, document_uri, root_uri, output)?;
    send_notification(transport, "exit", Value::Null)
}

fn send_request(
    transport: &mut impl Transport,
    versions: &mut VersionState,
    id: i64,
    method: &str,
    params: Value,
) -> Result<(), String> {
    versions.register_request(id, method)?;
    transport.send(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    }))
}

fn send_notification(
    transport: &mut impl Transport,
    method: &str,
    params: Value,
) -> Result<(), String> {
    transport.send(&json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    }))
}

fn wait_for_response(
    transport: &mut impl Transport,
    versions: &mut VersionState,
    expected_id: i64,
    document_uri: &str,
    root_uri: &str,
    output: &mut impl Write,
) -> Result<Value, String> {
    let deadline = Instant::now() + PROTOCOL_TIMEOUT;
    loop {
        let message = transport.receive(remaining(deadline)?)?;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if message.get("id").is_some() {
                respond_to_server_request(transport, versions, root_uri, &message, method)?;
            } else {
                handle_notification(&message, versions, document_uri, output)?;
            }
            continue;
        }

        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let freshness = versions.finish_request(id)?;
        if id != expected_id {
            return Err(format!(
                "received response id {id} while waiting for request id {expected_id}"
            ));
        }
        if let Freshness::Stale { observed, current } = freshness {
            return Err(format!(
                "stale response for request id {id} ignored: requested at {observed:?}, current is {current:?}"
            ));
        }
        if let Some(error) = message.get("error") {
            return Err(format!("rust-analyzer request {id} failed: {error}"));
        }
        return message
            .get("result")
            .cloned()
            .ok_or_else(|| format!("rust-analyzer response {id} has no result"));
    }
}

fn wait_for_diagnostics(
    transport: &mut impl Transport,
    versions: &mut VersionState,
    expected_version: i32,
    document_uri: &str,
    root_uri: &str,
    output: &mut impl Write,
) -> Result<(), String> {
    let deadline = Instant::now() + PROTOCOL_TIMEOUT;
    loop {
        let message = transport.receive(remaining(deadline)?)?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Err("received an unexpected response while waiting for diagnostics".to_owned());
        };
        if message.get("id").is_some() {
            respond_to_server_request(transport, versions, root_uri, &message, method)?;
            continue;
        }
        if let Some(version) = handle_notification(&message, versions, document_uri, output)?
            && version == expected_version
        {
            return Ok(());
        }
    }
}

fn handle_notification(
    message: &Value,
    versions: &VersionState,
    document_uri: &str,
    output: &mut impl Write,
) -> Result<Option<i32>, String> {
    if message.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
        return Ok(None);
    }
    let params = message
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| "publishDiagnostics params must be an object".to_owned())?;
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uri != document_uri {
        writeln!(
            output,
            "diagnostics for other document ignored: {}",
            crate::display::label(uri, 1024)
        )
        .map_err(|error| error.to_string())?;
        return Ok(None);
    }
    let version = params
        .get("version")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let count = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .ok_or_else(|| "publishDiagnostics diagnostics must be an array".to_owned())?
        .len();

    match versions.classify_diagnostics(version) {
        Freshness::Current(DocumentSnapshot::Version(version)) => {
            writeln!(output, "diagnostics v{version}: {count}")
                .map_err(|error| error.to_string())?;
            Ok(Some(version))
        }
        Freshness::Current(DocumentSnapshot::BeforeOpen) => {
            Err("versioned diagnostics arrived before didOpen".to_owned())
        }
        Freshness::Stale {
            observed: DocumentSnapshot::Version(observed),
            current: DocumentSnapshot::Version(current),
        } => {
            writeln!(
                output,
                "stale diagnostics v{observed} ignored; current is v{current}"
            )
            .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Freshness::Stale { observed, current } => {
            writeln!(
                output,
                "stale diagnostics at {observed:?} ignored; current is {current:?}"
            )
            .map_err(|error| error.to_string())?;
            Ok(None)
        }
        Freshness::Unversioned { current } => {
            writeln!(
                output,
                "unversioned diagnostics ignored; current is {current:?}"
            )
            .map_err(|error| error.to_string())?;
            Ok(None)
        }
    }
}

fn respond_to_server_request(
    transport: &mut impl Transport,
    versions: &VersionState,
    root_uri: &str,
    request: &Value,
    method: &str,
) -> Result<(), String> {
    let id = request
        .get("id")
        .cloned()
        .ok_or_else(|| "server request is missing id".to_owned())?;
    let _associated_document_state = versions.current();
    let result = match method {
        "workspace/configuration" => {
            let count = request
                .pointer("/params/items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => {
            json!([{"uri": root_uri, "name": "rustrace-spike"}])
        }
        "client/registerCapability" | "window/workDoneProgress/create" => Value::Null,
        _ => {
            return transport.send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unsupported server request: {method}")}
            }));
        }
    };
    transport.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn print_completion_summaries(result: &Value, output: &mut impl Write) -> Result<(), String> {
    let items = result
        .as_array()
        .or_else(|| result.get("items").and_then(Value::as_array))
        .ok_or_else(|| "completion response is neither an array nor CompletionList".to_owned())?;
    let mut valid = 0;
    for item in items.iter().take(100) {
        let Some(item) = item.as_object() else {
            continue;
        };
        let Some(label) = item.get("label").and_then(Value::as_str) else {
            continue;
        };
        let label = label.trim();
        if label.is_empty() || label.chars().any(char::is_control) {
            continue;
        }
        let kind = match item.get("kind") {
            Some(value) => match value.as_u64() {
                Some(kind) => Some(kind),
                None => continue,
            },
            None => None,
        };
        let detail = match item.get("detail") {
            Some(value) => match value.as_str() {
                Some(detail) => Some(detail),
                None => continue,
            },
            None => None,
        };
        let label = bounded_text(label, 120);
        let kind = kind
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let detail = detail
            .map(|value| format!(" — {}", bounded_text(value, 160)))
            .unwrap_or_default();
        writeln!(output, "completion: {label} [kind {kind}]{detail}")
            .map_err(|error| error.to_string())?;
        valid += 1;
        if valid == 20 {
            break;
        }
    }
    if valid == 0 {
        return Err("rust-analyzer returned no valid completion items".to_owned());
    }
    Ok(())
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    crate::display::label(text, max_chars)
}

#[test]
fn spike_display_exposes_nonprinting_without_changing_labels() {
    let label = "name\u{202e}text";
    let display = bounded_text(label, 120);
    assert_eq!(display, "name\\u{202e}text");
    assert_eq!(label.len(), 11);
}

fn drain_stderr_to_eof(reader: &mut impl Read, max_bytes: usize) -> io::Result<String> {
    let mut retained = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;

    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let retain_count = count.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..retain_count]);
        truncated |= retain_count < count;
    }

    let mut capture = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        if !capture.is_empty() && !capture.ends_with('\n') {
            capture.push('\n');
        }
        use std::fmt::Write as _;
        write!(
            capture,
            "…[stderr truncated after retaining {max_bytes} bytes]"
        )
        .map_err(io::Error::other)?;
    }
    Ok(capture)
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "timed out waiting for rust-analyzer after 30 seconds".to_owned())
}

pub fn run_real_spike(
    workspace: Option<&Path>,
    rust_analyzer: &OsStr,
    output: &mut impl Write,
) -> Result<(), String> {
    let workspace = PreparedWorkspace::new(workspace)?;
    let root_uri = file_uri(&workspace.root)?;
    let document_uri = file_uri(&workspace.document)?;
    writeln!(
        output,
        "workspace: {}",
        crate::display::label(&root_uri, 4096)
    )
    .map_err(|error| error.to_string())?;

    let mut transport = StdioTransport::spawn(rust_analyzer, &workspace.root)?;
    let protocol_result = run_protocol(&mut transport, &root_uri, &document_uri, output);
    let exit_result = transport.finish();
    protocol_result?;
    exit_result
}

struct PreparedWorkspace {
    root: PathBuf,
    document: PathBuf,
    temporary: bool,
}

impl PreparedWorkspace {
    fn new(supplied: Option<&Path>) -> Result<Self, String> {
        match supplied {
            Some(root) => Self::from_supplied(root),
            None => Self::temporary(),
        }
    }

    fn from_supplied(root: &Path) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|error| {
            format!("cannot open supplied workspace {}: {error}", root.display())
        })?;
        if !root.join("Cargo.toml").is_file() {
            return Err(format!(
                "supplied workspace {} has no Cargo.toml",
                root.display()
            ));
        }
        let document = [root.join("src/main.rs"), root.join("src/lib.rs")]
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                format!(
                    "supplied workspace {} needs src/main.rs or src/lib.rs",
                    root.display()
                )
            })?;
        Ok(Self {
            root,
            document,
            temporary: false,
        })
    }

    fn temporary() -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
            .as_nanos();
        let base = std::env::temp_dir();
        let mut root = None;
        for attempt in 0..100_u8 {
            let candidate = base.join(format!(
                "rustrace-ra-spike-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot create temporary workspace {}: {error}",
                        candidate.display()
                    ));
                }
            }
        }
        let root = root.ok_or_else(|| "cannot allocate a unique temporary workspace".to_owned())?;
        let document = root.join("src/main.rs");
        let setup = (|| -> io::Result<()> {
            fs::create_dir(root.join("src"))?;
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"rustrace-ra-spike\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
            )?;
            fs::write(&document, SOURCE_V1)?;
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = fs::remove_dir_all(&root);
            return Err(format!(
                "cannot initialize temporary Cargo workspace {}: {error}",
                root.display()
            ));
        }
        Ok(Self {
            root,
            document,
            temporary: true,
        })
    }
}

impl Drop for PreparedWorkspace {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn file_uri(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err(format!(
            "file URI path must be absolute: {}",
            path.display()
        ));
    }
    let text = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").map_err(|error| error.to_string())?;
        }
    }
    Ok(uri)
}

struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    stderr: Receiver<String>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    reaped: bool,
}

type IncomingMessage = Result<Value, String>;

fn incoming_message_channel() -> (mpsc::SyncSender<IncomingMessage>, Receiver<IncomingMessage>) {
    mpsc::sync_channel(INCOMING_QUEUE_CAPACITY)
}

impl StdioTransport {
    fn spawn(program: &OsStr, workspace: &Path) -> Result<Self, String> {
        Self::spawn_command(Command::new(program), workspace)
    }

    fn spawn_command(mut command: Command, workspace: &Path) -> Result<Self, String> {
        let program = command.get_program().to_os_string();
        let mut child = command
            .current_dir(workspace)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == ErrorKind::NotFound {
                    format!(
                        "rust-analyzer executable {:?} was not found; install it with `rustup component add rust-analyzer` or set RUST_ANALYZER",
                        program
                    )
                } else {
                    format!("cannot start rust-analyzer executable {:?}: {error}", program)
                }
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "cannot open rust-analyzer stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "cannot open rust-analyzer stdout".to_owned())?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| "cannot open rust-analyzer stderr".to_owned())?;

        let (message_sender, messages) = incoming_message_channel();
        thread::Builder::new()
            .name("rust-analyzer-stdout".to_owned())
            .spawn(move || {
                let mut stdout = stdout;
                loop {
                    match read_frame(&mut stdout, MAX_MESSAGE_BYTES) {
                        Ok(Some(message)) => {
                            if message_sender.send(Ok(message)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = message_sender
                                .send(Err("rust-analyzer closed its stdout".to_owned()));
                            break;
                        }
                        Err(error) => {
                            let _ = message_sender
                                .send(Err(format!("invalid rust-analyzer LSP frame: {error}")));
                            break;
                        }
                    }
                }
            })
            .map_err(|error| format!("cannot start rust-analyzer stdout reader: {error}"))?;

        let (stderr_sender, stderr_receiver) = mpsc::channel();
        let stderr_thread = thread::Builder::new()
            .name("rust-analyzer-stderr".to_owned())
            .spawn(move || {
                let message = drain_stderr_to_eof(&mut stderr, MAX_STDERR_BYTES)
                    .unwrap_or_else(|error| format!("cannot read rust-analyzer stderr: {error}"));
                let _ = stderr_sender.send(message);
            })
            .map_err(|error| format!("cannot start rust-analyzer stderr reader: {error}"))?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            messages,
            stderr: stderr_receiver,
            stderr_thread: Some(stderr_thread),
            reaped: false,
        })
    }

    #[cfg(test)]
    fn spawn_command_for_test(command: Command, workspace: &Path) -> Result<Self, String> {
        Self::spawn_command(command, workspace)
    }

    fn finish(&mut self) -> Result<(), String> {
        self.stdin.take();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    if status.success() {
                        return Ok(());
                    }
                    return Err(self.crash_message(status));
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    self.child.kill().map_err(|error| {
                        format!("rust-analyzer did not exit and kill failed: {error}")
                    })?;
                    let _ = self.child.wait();
                    self.reaped = true;
                    return Err(
                        "rust-analyzer did not exit within 5 seconds after exit notification"
                            .to_owned(),
                    );
                }
                Err(error) => return Err(format!("cannot wait for rust-analyzer: {error}")),
            }
        }
    }

    fn crash_message(&mut self, status: ExitStatus) -> String {
        let base = format!("rust-analyzer exited unexpectedly with {status}");
        match self.stderr.recv_timeout(STDERR_SYNC_TIMEOUT) {
            Ok(stderr) => {
                self.join_completed_stderr_thread();
                if stderr.is_empty() {
                    base
                } else {
                    format!("{base}: {stderr}")
                }
            }
            Err(RecvTimeoutError::Timeout) => format!(
                "{base}; stderr capture is still open after {} ms",
                STDERR_SYNC_TIMEOUT.as_millis()
            ),
            Err(RecvTimeoutError::Disconnected) => {
                self.join_completed_stderr_thread();
                format!("{base}; stderr capture ended without evidence")
            }
        }
    }

    fn join_completed_stderr_thread(&mut self) {
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return Ok(Some(status));
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => return Ok(None),
                Err(error) => {
                    return Err(format!("cannot inspect rust-analyzer process: {error}"));
                }
            }
        }
    }
}

impl Transport for StdioTransport {
    fn send(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "rust-analyzer stdin is closed".to_owned())?;
        write_frame(stdin, message)
            .map_err(|error| format!("cannot write to rust-analyzer: {error}"))
    }

    fn receive(&mut self, timeout: Duration) -> Result<Value, String> {
        match self.messages.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(reader_error)) => match self.wait_for_exit(EXIT_CONFIRM_TIMEOUT)? {
                Some(status) => Err(self.crash_message(status)),
                None => Err(reader_error),
            },
            Err(RecvTimeoutError::Timeout) => match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    Err(self.crash_message(status))
                }
                Ok(None) => Err(format!(
                    "timed out waiting for rust-analyzer after {} seconds",
                    timeout.as_secs()
                )),
                Err(error) => Err(format!("cannot inspect rust-analyzer process: {error}")),
            },
            Err(RecvTimeoutError::Disconnected) => match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    Err(self.crash_message(status))
                }
                Ok(None) => Err("rust-analyzer stdout reader stopped unexpectedly".to_owned()),
                Err(error) => Err(format!("cannot inspect rust-analyzer process: {error}")),
            },
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.stdin.take();
        if !self.reaped && self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Cursor, ErrorKind};

    use serde_json::json;

    use super::*;

    const FAKE_PROCESS_MODE: &str = "RUSTRACE_TEST_FAKE_PROCESS_MODE";

    #[test]
    fn fake_stderr_process() {
        let Some(mode) = std::env::var_os(FAKE_PROCESS_MODE) else {
            return;
        };
        match mode.to_str().expect("UTF-8 fake process mode") {
            "crash" => {
                let mut stderr = io::stderr().lock();
                stderr
                    .write_all(b"deterministic crash evidence\n")
                    .expect("write crash evidence");
                stderr
                    .write_all(&vec![b'x'; MAX_STDERR_BYTES + 1024])
                    .expect("write oversized crash evidence");
                stderr.flush().expect("flush crash evidence");
                std::process::exit(23);
            }
            "inherit-stderr" => {
                Command::new(std::env::current_exe().expect("current test executable"))
                    .args([
                        "--exact",
                        "rust_analyzer_spike::tests::fake_stderr_process",
                        "--nocapture",
                    ])
                    .env(FAKE_PROCESS_MODE, "hold-stderr")
                    .stdout(Stdio::null())
                    .spawn()
                    .expect("spawn stderr holder");
                eprintln!("parent crash evidence");
                std::process::exit(24);
            }
            "hold-stderr" => thread::sleep(Duration::from_secs(3)),
            other => panic!("unknown fake process mode: {other}"),
        }
    }

    fn fake_process_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args([
                "--exact",
                "rust_analyzer_spike::tests::fake_stderr_process",
                "--nocapture",
            ])
            .env(FAKE_PROCESS_MODE, mode);
        command
    }

    #[test]
    fn confirmed_crash_waits_for_complete_bounded_stderr_evidence() {
        let mut transport = StdioTransport::spawn_command_for_test(
            fake_process_command("crash"),
            &std::env::current_dir().expect("current directory"),
        )
        .expect("spawn fake crashing process");

        let error = transport
            .receive(Duration::from_secs(2))
            .expect_err("fake process must crash");

        assert!(error.contains("exit status: 23"), "{error}");
        assert!(error.contains("deterministic crash evidence"), "{error}");
        assert!(
            error.contains("stderr truncated after retaining 65536 bytes"),
            "{error}"
        );
    }

    #[test]
    fn confirmed_crash_does_not_wait_for_inherited_stderr_eof() {
        let started = Instant::now();
        let mut transport = StdioTransport::spawn_command_for_test(
            fake_process_command("inherit-stderr"),
            &std::env::current_dir().expect("current directory"),
        )
        .expect("spawn fake crashing process");

        let error = transport
            .receive(Duration::from_secs(2))
            .expect_err("fake process must crash");

        assert!(error.contains("exit status: 24"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("stderr capture is still open"), "{error}");
    }

    #[test]
    fn recorded_frame_round_trips_and_uses_content_length() {
        let message = json!({"jsonrpc": "2.0", "id": 7, "method": "shutdown"});
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &message).expect("write frame");

        let separator = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header terminator");
        let body = &bytes[separator + 4..];
        assert_eq!(
            &bytes[..separator],
            format!("Content-Length: {}", body.len()).as_bytes()
        );
        assert_eq!(
            read_frame(&mut Cursor::new(bytes), MAX_MESSAGE_BYTES).expect("read frame"),
            Some(message)
        );
    }

    #[test]
    fn incoming_lsp_queue_applies_backpressure_at_its_fixed_capacity() {
        let (sender, receiver) = incoming_message_channel();
        for id in 0..INCOMING_QUEUE_CAPACITY {
            sender
                .try_send(Ok(json!({"jsonrpc": "2.0", "id": id})))
                .expect("queue slot is available");
        }

        let overflow = sender
            .try_send(Ok(json!({"jsonrpc": "2.0", "id": "overflow"})))
            .expect_err("queue must apply backpressure");

        assert!(matches!(overflow, mpsc::TrySendError::Full(_)));

        drop(receiver);
        assert!(
            sender
                .send(Ok(json!({"jsonrpc": "2.0", "id": "cleanup"})))
                .is_err()
        );
    }

    #[test]
    fn frame_reader_rejects_oversized_and_ambiguous_frames() {
        let oversized = b"Content-Length: 6\r\n\r\n123456";
        let duplicate = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";

        let error = read_frame(&mut Cursor::new(oversized), 5).expect_err("size bound");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        let error = read_frame(&mut Cursor::new(duplicate), 5).expect_err("duplicate length");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn stderr_capture_drains_to_eof_and_retains_only_the_bounded_prefix() {
        let bytes = b"0123456789abcdef".to_vec();
        let mut reader = Cursor::new(bytes.clone());

        let capture = drain_stderr_to_eof(&mut reader, 5).expect("drain stderr");

        assert_eq!(reader.position(), bytes.len() as u64);
        assert_eq!(
            capture,
            "01234\n…[stderr truncated after retaining 5 bytes]"
        );
    }

    #[test]
    fn utf16_positions_handle_crlf_and_lone_cr_without_targeting_terminators() {
        let text = "naïve\r\n名称\r東京\n";
        assert_eq!(
            utf16_position(text, 6).unwrap(),
            Position {
                line: 0,
                character: 5
            }
        );
        assert!(
            utf16_position(text, 7).is_err(),
            "CRLF interior has no exact LSP position"
        );
        assert_eq!(
            utf16_position(text, 15).unwrap(),
            Position {
                line: 2,
                character: 0
            }
        );
        assert_eq!(
            utf16_position(text, text.len()).unwrap(),
            Position {
                line: 3,
                character: 0
            }
        );
    }

    #[test]
    fn utf16_positions_count_non_ascii_code_units_not_utf8_bytes() {
        let text = "let naïve = \"名称\";\nlet city = \"東京😀\"; city.";
        let byte_offset = text.len();

        let position = utf16_position(text, byte_offset).expect("valid boundary");

        assert_eq!(
            position,
            Position {
                line: 1,
                character: 24
            }
        );
        assert_ne!(
            position.character as usize,
            text.rsplit('\n').next().unwrap().len()
        );
        assert!(utf16_position(text, 7).is_err());
    }

    #[test]
    fn request_and_diagnostic_versions_are_never_mistaken_for_current() {
        let mut versions = VersionState::new();
        versions
            .register_request(1, "initialize")
            .expect("initialize request");
        assert_eq!(
            versions.finish_request(1).expect("initialize response"),
            Freshness::Current(DocumentSnapshot::BeforeOpen)
        );
        versions.open(1).expect("open version 1");
        versions
            .register_request(2, "textDocument/completion")
            .expect("completion request");
        versions.change(2).expect("change version 2");

        assert_eq!(
            versions.finish_request(2).expect("completion response"),
            Freshness::Stale {
                observed: DocumentSnapshot::Version(1),
                current: DocumentSnapshot::Version(2),
            }
        );
        assert_eq!(
            versions.classify_diagnostics(Some(1)),
            Freshness::Stale {
                observed: DocumentSnapshot::Version(1),
                current: DocumentSnapshot::Version(2),
            }
        );
        assert_eq!(
            versions.classify_diagnostics(None),
            Freshness::Unversioned {
                current: DocumentSnapshot::Version(2),
            }
        );
        assert_eq!(
            versions.classify_diagnostics(Some(2)),
            Freshness::Current(DocumentSnapshot::Version(2))
        );
        assert!(versions.change(2).is_err());
        assert!(versions.finish_request(99).is_err());
    }

    #[derive(Default)]
    struct FakeTransport {
        received: VecDeque<Value>,
        sent: Vec<Value>,
    }

    impl Transport for FakeTransport {
        fn send(&mut self, message: &Value) -> Result<(), String> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&mut self, _timeout: Duration) -> Result<Value, String> {
            self.received
                .pop_front()
                .ok_or_else(|| "fake transport exhausted".to_owned())
        }
    }

    #[test]
    fn fake_transport_runs_the_narrow_versioned_protocol_sequence() {
        let document_uri = "file:///tmp/spike/src/main.rs";
        let mut transport = FakeTransport {
            received: VecDeque::from([
                json!({"jsonrpc": "2.0", "id": 1, "result": {"capabilities": {"positionEncoding": "utf-16"}}}),
                json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": document_uri, "version": 1, "diagnostics": [{"message": "mismatched types"}]}}),
                json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": document_uri, "version": 1, "diagnostics": [{"message": "stale"}]}}),
                json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": document_uri, "version": 2, "diagnostics": [{"message": "expected expression"}]}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": [{"label": "len", "kind": 2, "detail": "fn len(&self) -> usize"}, {"kind": 2}]}),
                json!({"jsonrpc": "2.0", "id": 3, "result": null}),
            ]),
            sent: Vec::new(),
        };
        let mut default_encoding_transport = FakeTransport {
            received: transport.received.clone(),
            sent: Vec::new(),
        };
        default_encoding_transport.received[0]["result"]["capabilities"] = json!({});
        run_protocol(
            &mut default_encoding_transport,
            "file:///tmp/spike",
            document_uri,
            &mut Vec::new(),
        )
        .expect("omitted negotiation defaults to UTF-16");
        let mut output = Vec::new();

        run_protocol(
            &mut transport,
            "file:///tmp/spike",
            document_uri,
            &mut output,
        )
        .expect("protocol succeeds");

        assert_eq!(transport.sent, default_encoding_transport.sent);
        assert_eq!(
            transport.sent[0]["params"]["capabilities"]["general"]["positionEncodings"],
            json!(["utf-16"])
        );

        let methods = transport
            .sent
            .iter()
            .filter_map(|message| message.get("method").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "textDocument/didOpen",
                "textDocument/didChange",
                "textDocument/completion",
                "shutdown",
                "exit",
            ]
        );
        assert_eq!(transport.sent[3]["params"]["textDocument"]["version"], 2);
        assert_eq!(transport.sent[4]["params"]["position"]["character"], 35);
        let output = String::from_utf8(output).expect("UTF-8 output");
        assert!(output.contains("diagnostics v1: 1"));
        assert!(output.contains("stale diagnostics v1 ignored; current is v2"));
        assert!(output.contains("diagnostics v2: 1"));
        assert!(output.contains("completion: len [kind 2] — fn len(&self) -> usize"));
        assert!(!output.contains("completion: <missing>"));
    }

    #[test]
    fn unsupported_position_encoding_stops_before_document_sync() {
        for encoding in ["utf-8", "utf-32", "unknown"] {
            let mut transport = FakeTransport {
                received: VecDeque::from([
                    json!({"jsonrpc": "2.0", "id": 1, "result": {"capabilities": {"positionEncoding": encoding}}}),
                ]),
                sent: Vec::new(),
            };
            let error = run_protocol(
                &mut transport,
                "file:///tmp/spike",
                "file:///tmp/spike/src/main.rs",
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(error.contains("unsupported position encoding"));
            assert_eq!(transport.sent.len(), 1);
        }
    }

    #[test]
    fn completion_summaries_skip_malformed_consumed_fields() {
        let result = json!([
            "not an object",
            {"label": ""},
            {"label": "   "},
            {"label": "bad kind", "kind": {"arbitrary": "json"}},
            {"label": "bad detail", "detail": 42},
            {"label": "valid", "kind": 2, "detail": "fn valid()"}
        ]);
        let mut output = Vec::new();

        print_completion_summaries(&result, &mut output).expect("one valid item");

        assert_eq!(
            String::from_utf8(output).expect("UTF-8 output"),
            "completion: valid [kind 2] — fn valid()\n"
        );
    }

    #[test]
    fn malformed_completion_items_cannot_make_completion_succeed() {
        let result = json!([
            {"label": ""},
            {"label": "bad kind", "kind": [2]},
            {"label": "bad detail", "detail": false}
        ]);
        let mut output = Vec::new();

        let error = print_completion_summaries(&result, &mut output)
            .expect_err("all malformed items must fail");

        assert!(error.contains("no valid completion items"));
        assert!(output.is_empty());
    }
}
