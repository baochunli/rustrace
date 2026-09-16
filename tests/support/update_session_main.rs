//! Executable fixture compiled with the real recorder at the updated version.
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--fixture-session") {
        let session = rustrace::session::ProductionSession::start(
            std::path::Path::new(&args[2]), include_bytes!("manifest.toml"),
        ).unwrap();
        let metadata = session.metadata().clone();
        session.finalize("student").unwrap();
        println!("{}", serde_json::to_string(&metadata).unwrap());
    } else {
        let mut stdout = std::io::stdout().lock();
        std::process::exit(i32::from(rustrace::run_cli(
            args, &rustrace::SystemCommandRunner::default(), &mut stdout,
        )));
    }
}
