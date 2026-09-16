use std::env;
use std::io;
use std::process::ExitCode;

use rustrace::{SystemCommandRunner, run_cli};

fn main() -> ExitCode {
    let runner = SystemCommandRunner::default();
    let mut stdout = io::stdout().lock();
    ExitCode::from(run_cli(env::args(), &runner, &mut stdout))
}
