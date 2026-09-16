use rustrace::{
    CommandExecution, CommandRunner, ProbeCommand, run_cli, session::ProductionSession,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct UnexpectedRunner;

impl CommandRunner for UnexpectedRunner {
    fn run(&self, command: &ProbeCommand) -> CommandExecution {
        panic!(
            "version reporting unexpectedly ran {}",
            command.invocation()
        );
    }
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "rustrace-version-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        Self(root)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn manifest() -> &'static [u8] {
    br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Version reporting"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#
}

#[test]
fn version_cli_prints_exact_short_and_verbose_output_without_probing_tools() {
    let runner = UnexpectedRunner;
    let mut output = Vec::new();

    assert_eq!(run_cli(["rustrace", "--version"], &runner, &mut output), 0);
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!("rustrace {}\n", env!("CARGO_PKG_VERSION"))
    );

    let metadata = rustrace::version::version_metadata();
    let mut output = Vec::new();
    assert_eq!(
        run_cli(["rustrace", "--version", "--verbose"], &runner, &mut output),
        0
    );
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!(
            "rustrace {}\nbuild commit: {}\nevent format: {}\npackage format: {}\nassignment format: {}\ntarget: {}\n",
            metadata.client_version,
            metadata.build_commit(),
            metadata.event_format,
            metadata.package_format,
            metadata.assignment_format,
            metadata.target(),
        )
    );
}

#[test]
fn verbose_version_values_equal_fresh_production_session_metadata() {
    let directory = Directory::new();
    let session = ProductionSession::start(&directory.0, manifest()).unwrap();
    let metadata = session.metadata().clone();
    let receipt = session.finalize("student-1").unwrap();
    let persisted = receipt.manifest();
    let persisted_event_format = persisted
        .segments
        .last()
        .expect("started session persists one segment")
        .events
        .format_version;
    let reported = rustrace::version::version_metadata();
    assert_eq!(reported.client_version, metadata.client_version);
    assert_eq!(reported.build_identity, metadata.build_identity);
    assert_eq!(reported.event_format, persisted_event_format);
    assert_eq!(reported.client_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(reported.event_format, 1);
    assert_eq!(reported.package_format, persisted.format_version);
    assert_eq!(reported.package_format, 1);
    assert_eq!(
        persisted.format_version,
        rustrace_model::RPROV_FORMAT_VERSION_V1
    );
    assert_eq!(persisted.format_version, 1);
    assert_eq!(rustrace_model::assignment::SUPPORTED_FORMAT_VERSION, 2);
    assert_eq!(reported.assignment_format, 2);
    assert_eq!(
        reported.assignment_format,
        rustrace_model::assignment::SUPPORTED_FORMAT_VERSION
    );
    let runner = UnexpectedRunner;
    let mut output = Vec::new();

    assert_eq!(
        run_cli(["rustrace", "--version", "--verbose"], &runner, &mut output),
        0
    );
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| *line == "assignment format: 2")
            .count(),
        1
    );
    assert_eq!(
        output,
        format!(
            "rustrace {}\nbuild commit: {}\nevent format: {persisted_event_format}\npackage format: {}\nassignment format: 2\ntarget: {}\n",
            metadata.client_version,
            reported.build_commit(),
            persisted.format_version,
            reported.target(),
        )
    );
}
