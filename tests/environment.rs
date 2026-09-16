use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use rustrace::{
    CommandExecution, CommandRunner, DiagnosticLevel, ProbeCommand, Requirement,
    SystemCommandRunner, ToolStatus, probe_environment, run_cli,
};

struct FakeRunner {
    default: CommandExecution,
    outcomes: BTreeMap<String, CommandExecution>,
    calls: RefCell<Vec<String>>,
}

impl FakeRunner {
    fn all_successful() -> Self {
        Self {
            default: success("tool 1.0.0"),
            outcomes: BTreeMap::new(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn with_outcome(mut self, invocation: &str, outcome: CommandExecution) -> Self {
        self.outcomes.insert(invocation.to_owned(), outcome);
        self
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, command: &ProbeCommand) -> CommandExecution {
        let invocation = command.invocation();
        self.calls.borrow_mut().push(invocation.clone());
        self.outcomes
            .get(&invocation)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }
}

fn success(version: &str) -> CommandExecution {
    CommandExecution::Succeeded {
        stdout: format!("{version}\n"),
        stderr: String::new(),
    }
}

fn diagnostic<'a>(
    report: &'a rustrace::EnvironmentReport,
    component: &str,
) -> &'a rustrace::ComponentDiagnostic {
    report
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.component == component)
        .expect("component diagnostic")
}

#[test]
fn runs_the_exact_non_installing_version_commands_in_stable_order() {
    let runner = FakeRunner::all_successful();

    let report = probe_environment(&runner);

    assert_eq!(
        runner.calls.into_inner(),
        [
            "rustup --version",
            "rustc -vV",
            "cargo -V",
            "rust-analyzer --version",
            "rustfmt --version",
            "cargo clippy -V",
        ]
    );
    assert_eq!(report.diagnostics().len(), 6);
}

#[test]
fn required_not_found_is_an_actionable_blocker() {
    let runner =
        FakeRunner::all_successful().with_outcome("rustup --version", CommandExecution::NotFound);

    let report = probe_environment(&runner);
    let diagnostic = diagnostic(&report, "rustup");

    assert_eq!(diagnostic.requirement, Requirement::Required);
    assert_eq!(diagnostic.status, ToolStatus::NotFound);
    assert_eq!(diagnostic.level, DiagnosticLevel::Error);
    assert!(diagnostic.message.contains("executable was not found"));
    assert!(diagnostic.remediation.contains("https://rustup.rs"));
    assert!(report.has_blockers());
}

#[test]
fn optional_not_found_is_an_actionable_warning() {
    let runner =
        FakeRunner::all_successful().with_outcome("rustfmt --version", CommandExecution::NotFound);

    let report = probe_environment(&runner);
    let diagnostic = diagnostic(&report, "rustfmt");

    assert_eq!(diagnostic.requirement, Requirement::Optional);
    assert_eq!(diagnostic.status, ToolStatus::NotFound);
    assert_eq!(diagnostic.level, DiagnosticLevel::Warning);
    assert!(
        diagnostic
            .remediation
            .contains("rustup component add rustfmt")
    );
    assert!(!report.has_blockers());
}

#[test]
fn required_failure_preserves_bounded_status_and_stderr_evidence() {
    let runner = FakeRunner::all_successful().with_outcome(
        "cargo -V",
        CommandExecution::Failed {
            exit_code: Some(42),
            stdout: String::new(),
            stderr: format!("toolchain unavailable\n{}", "x".repeat(300)),
        },
    );

    let report = probe_environment(&runner);
    let diagnostic = diagnostic(&report, "Cargo");

    assert_eq!(diagnostic.status, ToolStatus::Failed);
    assert_eq!(diagnostic.level, DiagnosticLevel::Error);
    assert_eq!(diagnostic.evidence.exit_code, Some(42));
    assert!(
        diagnostic
            .evidence
            .stderr
            .as_deref()
            .expect("stderr evidence")
            .starts_with("toolchain unavailable")
    );
    assert!(
        diagnostic
            .evidence
            .stderr
            .as_ref()
            .expect("stderr evidence")
            .chars()
            .count()
            <= 161
    );
    assert!(report.has_blockers());
}

#[test]
fn optional_failure_preserves_status_and_stderr_as_a_warning() {
    let runner = FakeRunner::all_successful().with_outcome(
        "cargo clippy -V",
        CommandExecution::Failed {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "the clippy component is unavailable".to_owned(),
        },
    );

    let report = probe_environment(&runner);
    let diagnostic = diagnostic(&report, "Clippy");

    assert_eq!(diagnostic.status, ToolStatus::Failed);
    assert_eq!(diagnostic.level, DiagnosticLevel::Warning);
    assert_eq!(diagnostic.evidence.exit_code, Some(1));
    assert_eq!(
        diagnostic.evidence.stderr.as_deref(),
        Some("the clippy component is unavailable")
    );
    assert!(!report.has_blockers());
}

#[test]
fn timeout_is_explicit_and_preserves_partial_evidence() {
    let runner = FakeRunner::all_successful().with_outcome(
        "rustfmt --version",
        CommandExecution::TimedOut {
            timeout: Duration::from_millis(75),
            stdout: "partial version".to_owned(),
            stderr: "still running".to_owned(),
        },
    );

    let report = probe_environment(&runner);
    let diagnostic = diagnostic(&report, "rustfmt");

    assert_eq!(diagnostic.status, ToolStatus::TimedOut);
    assert_eq!(diagnostic.level, DiagnosticLevel::Warning);
    assert_eq!(diagnostic.evidence.timeout_ms, Some(75));
    assert_eq!(
        diagnostic.evidence.stdout.as_deref(),
        Some("partial version")
    );
    assert_eq!(diagnostic.evidence.stderr.as_deref(), Some("still running"));
    assert!(diagnostic.message.contains("timed out after 75 ms"));
}

#[test]
fn required_and_optional_successes_preserve_version_output() {
    let runner = FakeRunner::all_successful()
        .with_outcome("rustc -vV", success("rustc 1.85.0 (hash 2025-02-17)"))
        .with_outcome("rust-analyzer --version", success("rust-analyzer 1.85.0"));

    let report = probe_environment(&runner);
    let rustc = diagnostic(&report, "Rust compiler");
    let rust_analyzer = diagnostic(&report, "rust-analyzer");

    assert_eq!(rustc.status, ToolStatus::Available);
    assert_eq!(rustc.level, DiagnosticLevel::Info);
    assert_eq!(
        rustc.evidence.stdout.as_deref(),
        Some("rustc 1.85.0 (hash 2025-02-17)")
    );
    assert_eq!(rust_analyzer.status, ToolStatus::Available);
    assert_eq!(rust_analyzer.level, DiagnosticLevel::Info);
    assert_eq!(
        rust_analyzer.evidence.stdout.as_deref(),
        Some("rust-analyzer 1.85.0")
    );
}

#[test]
fn environment_cli_exit_code_tracks_required_unavailability_only() {
    let success = FakeRunner::all_successful();
    let required_not_found =
        FakeRunner::all_successful().with_outcome("rustup --version", CommandExecution::NotFound);
    let optional_not_found =
        FakeRunner::all_successful().with_outcome("rustfmt --version", CommandExecution::NotFound);
    let required_failure = FakeRunner::all_successful().with_outcome(
        "rustup --version",
        CommandExecution::Failed {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "rustup failed".to_owned(),
        },
    );
    let optional_failure = FakeRunner::all_successful().with_outcome(
        "rust-analyzer --version",
        CommandExecution::Failed {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "component unavailable".to_owned(),
        },
    );
    let mut output = Vec::new();

    assert_eq!(
        run_cli(["rustrace", "environment"], &success, &mut output),
        0
    );
    output.clear();
    assert_eq!(
        run_cli(
            ["rustrace", "environment"],
            &required_not_found,
            &mut output
        ),
        1
    );
    output.clear();
    assert_eq!(
        run_cli(
            ["rustrace", "environment"],
            &optional_not_found,
            &mut output
        ),
        0
    );
    output.clear();
    assert_eq!(
        run_cli(["rustrace", "environment"], &required_failure, &mut output),
        1
    );
    output.clear();
    assert_eq!(
        run_cli(["rustrace", "environment"], &optional_failure, &mut output),
        0
    );
    let output = String::from_utf8(output).expect("CLI output is UTF-8");
    assert!(output.contains("status: 1"));
    assert!(output.contains("stderr: component unavailable"));
}

#[test]
fn cli_rejects_unknown_commands() {
    let runner = FakeRunner::all_successful();
    let mut output = Vec::new();

    let exit_code = run_cli(["rustrace", "unknown"], &runner, &mut output);
    let output = String::from_utf8(output).expect("CLI output is UTF-8");

    assert_eq!(exit_code, 2);
    assert_eq!(
        output,
        "Usage: rustrace --version [--verbose] | rustrace environment | rustrace update | rustrace update --check | rustrace doctor assignment.rta [--workspace DIR] | rustrace doctor --write-ghostty-keys | rustrace work ... | rustrace replay PATH | rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH] | rustrace verify PATH [--reference PATH] | rustrace scan DIRECTORY [--output review.csv] [--reference assignment.rta] | rustrace status WORKSPACE | rustrace privacy WORKSPACE | rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta | rustrace cleanup WORKSPACE [--confirm] [--destroy-provenance]\n"
    );
}

#[cfg(unix)]
#[test]
fn system_runner_kills_and_reaps_a_probe_at_its_deadline() {
    let runner = SystemCommandRunner::with_limits(Duration::from_millis(50), 64);
    let command = ProbeCommand {
        program: "sh",
        args: &[
            "-c",
            "printf partial; printf warning >&2; while :; do :; done",
        ],
    };
    let started = Instant::now();

    let execution = runner.run(&command);

    assert!(started.elapsed() < Duration::from_secs(2));
    let CommandExecution::TimedOut {
        timeout,
        stdout,
        stderr,
    } = execution
    else {
        panic!("expected timed-out execution");
    };
    assert_eq!(timeout, Duration::from_millis(50));
    assert_eq!(stdout, "partial");
    assert_eq!(stderr, "warning");
}

#[cfg(unix)]
#[test]
fn system_runner_caps_stdout_and_stderr_while_draining_them() {
    const CAPTURE_LIMIT: usize = 64;
    let runner = SystemCommandRunner::with_limits(Duration::from_secs(5), CAPTURE_LIMIT);
    let command = ProbeCommand {
        program: "sh",
        args: &[
            "-c",
            "chunk=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=0; while [ \"$i\" -lt 4096 ]; do printf %s \"$chunk\"; printf %s \"$chunk\" >&2; i=$((i + 1)); done",
        ],
    };

    let execution = runner.run(&command);

    let CommandExecution::Succeeded { stdout, stderr } = execution else {
        panic!("expected successful execution");
    };
    assert_eq!(stdout.len(), CAPTURE_LIMIT);
    assert_eq!(stderr.len(), CAPTURE_LIMIT);
}

#[cfg(unix)]
#[test]
fn system_runner_terminates_inherited_children_on_success_and_timeout() {
    let runner = SystemCommandRunner::with_limits(Duration::from_millis(500), 128);
    const PROBES: &[ProbeCommand] = &[
        ProbeCommand {
            program: "sh",
            args: &["-c", "sleep 30 & printf '%s' \"$!\"; exit 0"],
        },
        ProbeCommand {
            program: "sh",
            args: &["-c", "sleep 30 & printf '%s' \"$!\"; while :; do :; done"],
        },
    ];
    for probe in PROBES {
        let script = probe.args[1];
        let began = Instant::now();
        let result = runner.run(probe);
        let stdout = match result {
            CommandExecution::Succeeded { stdout, .. } if script.ends_with("exit 0") => stdout,
            CommandExecution::TimedOut { stdout, .. } if !script.ends_with("exit 0") => stdout,
            unexpected => panic!("unexpected probe result: {unexpected:?}"),
        };
        assert!(began.elapsed() < Duration::from_secs(3));
        let pid: i32 = stdout.parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let absent = unsafe { libc::kill(pid, 0) } != 0;
            // Linux containers may not promptly reap an orphan zombie; it is
            // already terminated and cannot retain pipes or execute code.
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.split_once(") ")
                    .is_some_and(|(_, state)| state.starts_with("Z "))
            });
            if absent || zombie {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "probe left running descendant {pid}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
