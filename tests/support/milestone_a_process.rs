//! Test-only process-boundary driver for the temporary Milestone A slice.

use std::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;

use rustrace::editor::Movement;
use rustrace::milestone_a::{OneFileSession, ReplayReport, verify_and_replay_from_directory};
use rustrace_model::{DocumentId, SessionId};
use serde::Serialize;
use serde::ser::{SerializeSeq, Serializer};
use serde_json::{Value, json};

pub(crate) fn run(
    arguments: impl IntoIterator<Item = OsString>,
    writer: &mut impl std::io::Write,
) -> Result<(), Box<dyn Error>> {
    let mut arguments = arguments.into_iter();
    let mode = required(&mut arguments, "mode")?;
    let directory = PathBuf::from(required(&mut arguments, "directory")?);
    let session_id = SessionId::new(
        required(&mut arguments, "session ID")?
            .into_string()
            .map_err(|_| "session ID must be valid UTF-8")?,
    )?;
    match mode.to_str() {
        Some("record") => {
            let marker = PathBuf::from(required(&mut arguments, "non-execution marker")?);
            reject_extra(arguments)?;
            record(&directory, session_id, marker)?;
        }
        Some("verify") => {
            reject_extra(arguments)?;
            verify(&directory, &session_id, writer)?;
        }
        _ => return Err("mode must be `record` or `verify`".into()),
    }
    Ok(())
}

fn record(
    directory: &PathBuf,
    session_id: SessionId,
    marker: PathBuf,
) -> Result<(), Box<dyn Error>> {
    let starter = format!("fn main() {{ std::fs::write({marker:?}, b\"executed\").unwrap(); }}\n");
    let mut session = OneFileSession::start(directory, session_id, &starter)?;
    session.move_cursor(Movement::DocumentEnd, false)?;
    session.paste("\n// héllo 🦀\n")?;
    session.insert_char('x')?;
    session.undo()?;
    session.redo()?;
    session.finish()?;
    Ok(())
}

fn verify(
    directory: &PathBuf,
    session_id: &SessionId,
    writer: &mut impl std::io::Write,
) -> Result<(), Box<dyn Error>> {
    let report = verify_and_replay_from_directory(directory, session_id)?;
    write_report(&report, writer, false)
}

fn write_report(
    report: &ReplayReport,
    writer: &mut impl std::io::Write,
    terminal: bool,
) -> Result<(), Box<dyn Error>> {
    if terminal {
        // Borrow source and visit steps only as the bounded serializer asks for
        // them. Never build the owned machine projection for human display.
        let preview = TerminalReport {
            session_id: report.session_id().as_str(),
            source: std::str::from_utf8(report.source_bytes())?,
            event_count: report.event_count(),
            checkpoint_count: report.checkpoint_count(),
            finalized: report.finalized(),
            final_event_hash: report.final_event_hash().to_string(),
            final_workspace_hash: report.final_workspace_hash().to_string(),
            steps: TerminalSteps(report),
        };
        writeln!(writer, "{}", rustrace::display::json_preview(&preview)?)?;
        return Ok(());
    }
    let document_id = DocumentId::new("main.rs")?;
    let steps = report
        .steps()
        .iter()
        .map(|step| {
            let document = step
                .state
                .document(&document_id)
                .ok_or("replayed state has no main.rs document")?;
            Ok::<Value, Box<dyn Error>>(json!({
                "sequence": step.sequence,
                "text": document.text(),
                "version": document.version(),
                "anchor_byte": document.selection().anchor_byte,
                "active_byte": document.selection().active_byte,
                "document_hash": document.content_hash().to_string(),
                "workspace_hash": step.state.workspace_hash().to_string(),
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let output = json!({
        "session_id": report.session_id().as_str(),
        "source": std::str::from_utf8(report.source_bytes())?,
        "event_count": report.event_count(),
        "checkpoint_count": report.checkpoint_count(),
        "finalized": report.finalized(),
        "final_event_hash": report.final_event_hash().to_string(),
        "final_workspace_hash": report.final_workspace_hash().to_string(),
        "steps": steps,
    });
    // Machine/evidence output retains its exact serializer and byte values.
    serde_json::to_writer(writer, &output)?;
    Ok(())
}

#[derive(Serialize)]
struct TerminalReport<'a> {
    session_id: &'a str,
    source: &'a str,
    event_count: u64,
    checkpoint_count: u64,
    finalized: bool,
    final_event_hash: String,
    final_workspace_hash: String,
    steps: TerminalSteps<'a>,
}

struct TerminalSteps<'a>(&'a ReplayReport);

impl Serialize for TerminalSteps<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Step<'a> {
            sequence: u64,
            text: &'a str,
            version: u64,
            anchor_byte: u64,
            active_byte: u64,
            document_hash: String,
            workspace_hash: String,
        }
        let document_id = DocumentId::new("main.rs").map_err(serde::ser::Error::custom)?;
        let mut sequence = serializer.serialize_seq(Some(self.0.steps().len()))?;
        for step in self.0.steps() {
            let document = step.state.document(&document_id).ok_or_else(|| {
                serde::ser::Error::custom("replayed state has no main.rs document")
            })?;
            sequence.serialize_element(&Step {
                sequence: step.sequence,
                text: document.text(),
                version: document.version(),
                anchor_byte: document.selection().anchor_byte,
                active_byte: document.selection().active_byte,
                document_hash: document.content_hash().to_string(),
                workspace_hash: step.state.workspace_hash().to_string(),
            })?;
        }
        sequence.end()
    }
}

fn required(
    arguments: &mut impl Iterator<Item = OsString>,
    label: &'static str,
) -> Result<OsString, Box<dyn Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("missing {label}").into())
}

fn reject_extra(mut arguments: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    if arguments.next().is_some() {
        Err("unexpected extra argument".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;
    thread_local! {
        static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
    }
    fn count(bytes: usize) {
        let _ = ALLOCATED.try_with(|total| {
            if let Some(current) = total.get() {
                total.set(Some(current + bytes));
            }
        });
    }
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            count(size);
            unsafe { System.realloc(pointer, layout, size) }
        }
    }
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn terminal_report_projection_is_bounded_and_machine_bytes_are_exact() {
        // Exercise a complete preview, truncation within lazy steps, and
        // truncation within source before any step is serialized.
        for source_size in [8, 4096, 256 * 1024] {
            check_report(source_size);
        }
    }

    fn check_report(source_size: usize) {
        let root = Fixture(std::env::temp_dir().join(format!(
            "t82-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        let id = SessionId::new("bounded-report").unwrap();
        let source = "x".repeat(source_size);
        let mut session = OneFileSession::start(&root.0, id.clone(), &source).unwrap();
        for _ in 0..6 {
            session.move_cursor(Movement::Right, false).unwrap();
        }
        session.finish().unwrap();
        let report = verify_and_replay_from_directory(&root.0, &id).unwrap();
        assert_eq!(report.steps().len(), 9);
        let original = report.clone();

        ALLOCATED.with(|total| total.set(Some(0)));
        let mut preview = Vec::new();
        let result = write_report(&report, &mut preview, true);
        let allocated = ALLOCATED.with(|total| total.replace(None).unwrap());
        result.unwrap();
        assert!(
            allocated <= 512 * 1024,
            "terminal report allocated {allocated} bytes"
        );
        assert!(preview.len() <= 17 * 1024);
        let preview = String::from_utf8(preview).unwrap();
        assert_eq!(preview.contains("[display truncated]"), source_size > 8);
        assert_eq!(report, original);

        let mut machine = Vec::new();
        write_report(&report, &mut machine, false).unwrap();
        let document_id = DocumentId::new("main.rs").unwrap();
        let expected = json!({
            "session_id": id.as_str(), "source": source,
            "event_count": report.event_count(), "checkpoint_count": report.checkpoint_count(),
            "finalized": report.finalized(), "final_event_hash": report.final_event_hash().to_string(),
            "final_workspace_hash": report.final_workspace_hash().to_string(),
            "steps": report.steps().iter().map(|step| {
                let document = step.state.document(&document_id).unwrap();
                json!({"sequence": step.sequence, "text": document.text(), "version": document.version(),
                    "anchor_byte": document.selection().anchor_byte, "active_byte": document.selection().active_byte,
                    "document_hash": document.content_hash().to_string(), "workspace_hash": step.state.workspace_hash().to_string()})
            }).collect::<Vec<_>>()
        });
        assert_eq!(machine, serde_json::to_vec(&expected).unwrap());
        if source_size == 8 {
            assert_eq!(serde_json::from_str::<Value>(&preview).unwrap(), expected);
        }
        assert_eq!(report.source_bytes(), source.as_bytes());
    }
}
