#[cfg(test)]
extern crate self as rustrace;

pub mod cargo_policy;
mod command_process;
pub mod config;
mod console;
pub mod diagnostics;
pub mod display;
mod doctor;
pub mod editor;
mod environment;
pub mod ghostty;
mod language_service;
pub mod milestone_a;
pub mod process_indicators;
pub mod replay_tui;
pub mod review_flags;
pub mod rust_analyzer_spike;
pub mod scan;
pub mod session;
pub mod session_fixture;
pub mod toolchain;
pub mod tui;
pub mod update;
pub mod verify;
pub mod version;
pub mod work;
mod work_editor;

use std::io::Write;

pub use environment::{
    CommandExecution, CommandRunner, ComponentDiagnostic, DEFAULT_CAPTURE_LIMIT_BYTES,
    DEFAULT_PROBE_TIMEOUT, DiagnosticLevel, EnvironmentReport, ExecutionEvidence, ProbeCommand,
    Requirement, SystemCommandRunner, ToolStatus, probe_environment,
};

const USAGE: &str = "Usage: rustrace --version [--verbose] | rustrace environment | rustrace update | rustrace update --check | rustrace doctor assignment.rta [--workspace DIR] | rustrace doctor --write-ghostty-keys | rustrace work ... | rustrace replay PATH | rustrace submit WORKSPACE --student-id ID [--allow-incomplete] [--output PATH] | rustrace verify PATH [--reference PATH] | rustrace scan DIRECTORY [--output review.csv] [--reference assignment.rta] | rustrace status WORKSPACE | rustrace privacy WORKSPACE | rustrace revise PARENT_WORKSPACE NEW_WORKSPACE assignment.rta | rustrace cleanup WORKSPACE [--confirm] [--destroy-provenance]";

/// Runs the deliberately narrow Phase 0 command surface.
///
/// The command runner and output are injected so diagnostics can be tested
/// without depending on the machine running the tests.
pub fn run_cli<I, S, W>(args: I, runner: &dyn CommandRunner, output: &mut W) -> u8
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    W: Write,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let command = args.next().map(|value| value.as_ref().to_owned());

    if command.as_deref() == Some("--version") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        let verbose = match args.as_slice() {
            [] => false,
            [argument] if argument == "--verbose" => true,
            _ => {
                let _ = writeln!(output, "{USAGE}");
                return 2;
            }
        };
        version::write_version(output, verbose);
        return 0;
    }

    if command.as_deref() == Some("update") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return update::run_update(&args, output);
    }

    if command.as_deref() == Some("doctor") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match doctor::run_doctor(&args, output) {
            Ok(code) => code,
            Err(error) => {
                let _ = writeln!(output, "{}", display::label(&error, 4096));
                2
            }
        };
    }
    if command.as_deref() == Some("work") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match work::run_work(&args, output) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(
                    output,
                    "work stopped: {}",
                    display::label_fmt(format_args!("{error}"), 4096)
                );
                1
            }
        };
    }
    if command.as_deref() == Some("submit") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match session::run_submit(&args, output) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(
                    output,
                    "submit stopped: {}",
                    display::label_fmt(format_args!("{error}"), 4096)
                );
                1
            }
        };
    }
    if command.as_deref() == Some("privacy") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match session::run_privacy(&args, output) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(
                    output,
                    "privacy stopped: {}",
                    display::label_fmt(format_args!("{error}"), 4096)
                );
                1
            }
        };
    }
    if matches!(command.as_deref(), Some("status" | "revise" | "cleanup")) {
        let command = command.expect("matched command");
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        let result = match command.as_str() {
            "status" => session::run_status(&args, output),
            "revise" => session::run_revise(&args, output),
            "cleanup" => session::run_cleanup(&args, output),
            _ => unreachable!(),
        };
        return match result {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(
                    output,
                    "{command} stopped: {}",
                    display::label_fmt(format_args!("{error}"), 4096)
                );
                1
            }
        };
    }
    if command.as_deref() == Some("verify") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match verify::run_verify(&args, output) {
            Ok(code) => code,
            Err(error) => {
                let _ = writeln!(output, "{}", display::label(&error, 4096));
                2
            }
        };
    }
    if command.as_deref() == Some("scan") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match scan::run_scan(&args, output) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(output, "scan stopped: {}", display::label(&error, 4096));
                1
            }
        };
    }
    if command.as_deref() == Some("replay") {
        let args = args.map(|arg| arg.as_ref().to_owned()).collect::<Vec<_>>();
        return match replay_tui::run_replay(&args) {
            Ok(()) => 0,
            Err(error) => {
                let _ = writeln!(output, "{}", display::label(&error, 4096));
                1
            }
        };
    }
    if command.as_deref() != Some("environment") || args.next().is_some() {
        let _ = writeln!(output, "{USAGE}");
        return 2;
    }

    let report = probe_environment(runner);
    write_report(&report, output);

    if report.has_blockers() { 1 } else { 0 }
}

fn write_report<W: Write>(report: &EnvironmentReport, output: &mut W) {
    for diagnostic in report.diagnostics() {
        let _ = writeln!(
            output,
            "[{}] {} ({}): {}",
            diagnostic.level.as_str(),
            diagnostic.component,
            diagnostic.requirement.as_str(),
            display::label(&diagnostic.message, 2048)
        );

        if diagnostic.status == ToolStatus::Failed {
            match diagnostic.evidence.exit_code {
                Some(exit_code) => {
                    let _ = writeln!(output, "  status: {exit_code}");
                }
                None => {
                    let _ = writeln!(output, "  status: unavailable");
                }
            }
        }
        if diagnostic.status == ToolStatus::TimedOut {
            let timeout_ms = diagnostic
                .evidence
                .timeout_ms
                .expect("timed-out diagnostics include a deadline");
            let _ = writeln!(output, "  timeout: {timeout_ms} ms");
        }
        if let Some(stdout) = &diagnostic.evidence.stdout {
            let _ = writeln!(output, "  stdout: {}", display::label(stdout, 2048));
        }
        if let Some(stderr) = &diagnostic.evidence.stderr {
            let _ = writeln!(output, "  stderr: {}", display::label(stderr, 2048));
        }
        if diagnostic.status != ToolStatus::Available {
            let _ = writeln!(output, "  help: {}", diagnostic.remediation);
        }
    }

    let required_issues = report.required_issue_count();
    let optional_issues = report.optional_issue_count();

    if required_issues > 0 {
        let noun = if required_issues == 1 {
            "tool is"
        } else {
            "tools are"
        };
        let _ = writeln!(
            output,
            "Environment check failed: {required_issues} required {noun} unavailable."
        );
    } else if optional_issues > 0 {
        let noun = if optional_issues == 1 {
            "tool"
        } else {
            "tools"
        };
        let _ = writeln!(
            output,
            "Environment check passed with {optional_issues} optional {noun} unavailable."
        );
    } else {
        let _ = writeln!(output, "Environment check passed.");
    }
}
