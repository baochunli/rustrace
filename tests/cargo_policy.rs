use rustrace::cargo_policy::{
    CargoAction, ConsoleCommand, ResolvedTools, parse_console_command, prepare, prepare_console,
};
use rustrace_model::WorkspacePath;
use std::{path::PathBuf, process::Command};

#[test]
fn console_literal_grammar_accepts_only_fixed_actions_and_exact_run_redirection() {
    for (input, action, argv, stdin, stdout) in [
        (
            "cargo build",
            CargoAction::Build,
            vec!["cargo", "build"],
            None,
            None,
        ),
        (
            "cargo check",
            CargoAction::Check,
            vec!["cargo", "check"],
            None,
            None,
        ),
        (
            "cargo test",
            CargoAction::Test,
            vec!["cargo", "test"],
            None,
            None,
        ),
        (
            "cargo clippy",
            CargoAction::Clippy,
            vec!["cargo", "clippy"],
            None,
            None,
        ),
        (
            "cargo run",
            CargoAction::Run,
            vec!["cargo", "run"],
            None,
            None,
        ),
        (
            "cargo run --release < nested/in.txt > out.bin",
            CargoAction::Run,
            vec!["cargo", "run", "--release"],
            Some("nested/in.txt"),
            Some("out.bin"),
        ),
        (
            "cargo run > out.bin < in.txt",
            CargoAction::Run,
            vec!["cargo", "run"],
            Some("in.txt"),
            Some("out.bin"),
        ),
        (
            "cargo doc",
            CargoAction::Doc,
            vec!["cargo", "doc"],
            None,
            None,
        ),
        (
            "cargo add serde",
            CargoAction::Add,
            vec!["cargo", "add", "serde"],
            None,
            None,
        ),
        (
            "cargo add serde@1.0.229",
            CargoAction::Add,
            vec!["cargo", "add", "serde@1.0.229"],
            None,
            None,
        ),
        (
            "cargo remove serde_json",
            CargoAction::Remove,
            vec!["cargo", "remove", "serde_json"],
            None,
            None,
        ),
        (
            "cargo update",
            CargoAction::Update,
            vec!["cargo", "update"],
            None,
            None,
        ),
    ] {
        let parsed =
            parse_console_command(input).unwrap_or_else(|error| panic!("{input}: {error}"));
        assert_eq!(parsed.action, action, "{input}");
        assert_eq!(parsed.argv, argv, "{input}");
        assert_eq!(
            parsed.stdin.as_ref().map(|path| path.as_str()),
            stdin,
            "{input}"
        );
        assert_eq!(
            parsed.stdout.as_ref().map(|path| path.as_str()),
            stdout,
            "{input}"
        );
    }
}

#[test]
fn console_literal_grammar_rejects_shell_syntax_flags_and_malformed_routes() {
    for input in [
        "",
        "cargo",
        "cargo fmt",
        "cargo clean",
        "rustc main.rs",
        "cargo build --release",
        "cargo test > out",
        "cargo run --release --release",
        "cargo run x",
        "cargo run -- x",
        "cargo run <",
        "cargo run >",
        "cargo run < in < other",
        "cargo run > out trailing",
        "cargo run >> out",
        "cargo run > >>out",
        "cargo run > foo>bar",
        "cargo run < foo>bar",
        "cargo run < <<EOF",
        "cargo run > 2>err",
        "cargo run 2> err",
        "cargo run << in",
        "cargo run < /tmp/in",
        "cargo run < ../in",
        "cargo run < 'in'",
        "cargo run < in\\ file",
        "cargo run | tee out",
        "cargo run && echo x",
        "cargo run > $OUT",
        "cargo run > *.txt",
        "cargo run < same > same",
        "cargo run\ncheck",
        "cargo doc --open",
        "cargo add",
        "cargo add serde@1",
        "cargo add serde@1.2",
        "cargo add serde@01.2.3",
        "cargo add serde@1.2.3 --features derive",
        "cargo add ../serde",
        "cargo add serde --path ../serde",
        "cargo add serde --git https://example.invalid/repo",
        "cargo add serde --registry private",
        "cargo add serde@>=1.0.0",
        "cargo add serde@1.0.0,2.0.0",
        "cargo add ser/de",
        "cargo add -serde",
        "cargo add 1serde",
        "cargo add serde🙂",
        "cargo remove",
        "cargo remove serde@1.0.0",
        "cargo remove serde --dev",
        "cargo update serde",
        "cargo update --precise 1.0.0",
    ] {
        assert!(parse_console_command(input).is_err(), "accepted {input:?}");
    }
}

#[test]
fn console_test_grammar_preserves_every_bounded_filter_and_output_form() {
    let longest_filter = "a".repeat(256);
    let padded = format!(
        "{}cargo  test{}",
        " ".repeat(2048),
        " ".repeat(4096 - 2048 - "cargo  test".len())
    );
    let cases = [
        "cargo test legal_moves".to_owned(),
        "cargo test tests::legal_moves".to_owned(),
        "cargo test tests::".to_owned(),
        "cargo test 1".to_owned(),
        "cargo test legal-moves".to_owned(),
        "cargo test -- --nocapture".to_owned(),
        "cargo test -- --no-capture".to_owned(),
        "cargo test -- --show-output".to_owned(),
        "cargo test tests::legal_moves -- --nocapture".to_owned(),
        "cargo test tests::legal_moves -- --no-capture".to_owned(),
        "cargo test tests::legal_moves -- --show-output".to_owned(),
        format!("cargo test {longest_filter}"),
        padded,
    ];

    for input in cases {
        let expected = input.split_ascii_whitespace().collect::<Vec<_>>();
        let parsed = parse_console_command(&input)
            .unwrap_or_else(|error| panic!("rejected {input:?}: {error}"));
        assert_eq!(parsed.action, CargoAction::Test, "{input:?}");
        assert_eq!(parsed.argv, argv(&expected), "{input:?}");
        assert_eq!(parsed.stdin, None, "{input:?}");
        assert_eq!(parsed.stdout, None, "{input:?}");
    }
}

#[test]
fn console_test_grammar_rejects_every_escape_and_byte_overflow() {
    let oversized_filter = format!("cargo test {}", "a".repeat(257));
    let oversized_line = format!("cargo test{}", " ".repeat(4097 - "cargo test".len()));
    let mut cases = vec![
        "cargo test one two".to_owned(),
        "cargo test --".to_owned(),
        "cargo test legal_moves --".to_owned(),
        "cargo test --nocapture".to_owned(),
        "cargo test -legal_moves".to_owned(),
        "cargo test -- --nocapture legal_moves".to_owned(),
        "cargo test -- legal_moves".to_owned(),
        "cargo test -- --nocapture --show-output".to_owned(),
        "cargo test -- --nocapture --nocapture".to_owned(),
        "cargo test -- -- --show-output".to_owned(),
        "cargo test -- --nocapture=true".to_owned(),
        "cargo test -- --exact".to_owned(),
        "cargo test -- --ignored".to_owned(),
        "cargo test -- --test-threads=1".to_owned(),
        "cargo test --locked".to_owned(),
        "cargo test --release".to_owned(),
        "cargo test --package example".to_owned(),
        "cargo test --manifest-path other/Cargo.toml".to_owned(),
        "cargo test --message-format=json".to_owned(),
        "cargo test -- --locked".to_owned(),
        "cargo test legal_moves > out".to_owned(),
        "cargo test legal_moves < in".to_owned(),
        "cargo test legal_moves>out".to_owned(),
        "cargo test 'legal_moves'".to_owned(),
        "cargo test legal*".to_owned(),
        "cargo test $(name)".to_owned(),
        "cargo test legal_moves; cargo run".to_owned(),
        "cargo test tests/legal_moves".to_owned(),
        "cargo test tests.legal_moves".to_owned(),
        "cargo test légal_moves".to_owned(),
        "cargo test\tlegal_moves".to_owned(),
    ];
    cases.extend([oversized_filter, oversized_line]);

    for input in cases {
        assert!(
            parse_console_command(&input).is_err(),
            "accepted invalid Test command {input:?}"
        );
    }
}

#[test]
fn dependency_name_and_semver_bounds_are_enforced() {
    let longest_name = format!("cargo add {}", "a".repeat(64));
    assert!(parse_console_command(&longest_name).is_ok());
    let oversized_name = format!("cargo add {}", "a".repeat(65));
    assert!(parse_console_command(&oversized_name).is_err());
    assert!(parse_console_command("cargo add serde@1.2.3-alpha.1+build.5").is_ok());
    for input in [
        "cargo add serde@1.2.3-01",
        "cargo add serde@1.2.3-",
        "cargo add serde@1.2.3+",
        "cargo add serde@1.2.3+build+other",
    ] {
        assert!(parse_console_command(input).is_err(), "accepted {input:?}");
    }
}

fn tools() -> ResolvedTools {
    ResolvedTools {
        rustup: "/trusted/rustup".into(),
        selection: "pinned".into(),
        cargo: "/trusted/cargo".into(),
        rustc: "/trusted/rustc".into(),
        rustdoc: "/trusted/rustdoc".into(),
        cargo_clippy: Some("/trusted/cargo-clippy".into()),
        formatter: Some(("/trusted/cargo-fmt".into(), "/trusted/rustfmt".into())),
    }
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}

#[test]
fn accepted_contract_forms_prepare_exact_tools_without_spawning() {
    for (action, input, executable, args) in [
        (
            CargoAction::Check,
            vec!["cargo", "check", "--locked"],
            "cargo",
            vec!["check", "--message-format=json", "--locked"],
        ),
        (
            CargoAction::Test,
            vec!["cargo", "test"],
            "cargo",
            vec!["test", "--message-format=json", "--locked"],
        ),
        (
            CargoAction::Run,
            vec!["cargo", "run", "--offline"],
            "cargo",
            vec!["run", "--message-format=json", "--locked"],
        ),
        (
            CargoAction::Clippy,
            vec!["cargo", "clippy", "--locked", "--", "-D", "warnings"],
            "cargo-clippy",
            vec![
                "clippy",
                "--message-format=json",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ),
        (
            CargoAction::Format,
            vec!["cargo", "fmt"],
            "cargo-fmt",
            vec!["fmt"],
        ),
        (
            CargoAction::Doc,
            vec!["cargo", "doc"],
            "cargo",
            vec!["doc", "--message-format=json", "--locked"],
        ),
        (
            CargoAction::Add,
            vec!["cargo", "add", "serde@1.0.229"],
            "cargo",
            vec!["add", "serde@1.0.229"],
        ),
        (
            CargoAction::Remove,
            vec!["cargo", "remove", "serde"],
            "cargo",
            vec!["remove", "serde"],
        ),
        (
            CargoAction::Update,
            vec!["cargo", "update"],
            "cargo",
            vec!["update"],
        ),
    ] {
        let prepared = prepare(
            action,
            &argv(&input),
            &tools(),
            &PathBuf::from("/workspace"),
        )
        .unwrap();
        let mut expected = vec![
            "run".to_owned(),
            "pinned".into(),
            format!("/trusted/{executable}"),
        ];
        expected.extend(argv(&args));
        assert_eq!(prepared.command.get_program(), "/trusted/rustup");
        assert_eq!(
            prepared.command.get_args().collect::<Vec<_>>(),
            expected
                .iter()
                .map(std::ffi::OsStr::new)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            prepared.command.get_current_dir(),
            Some(std::path::Path::new("/workspace"))
        );
        assert_eq!(prepared.summary.policy_version, 1);
        assert!(prepared.summary.retained_names.len() <= 7);
        assert!(
            prepared
                .command
                .get_envs()
                .all(|(name, _)| name != "CARGO_NET_OFFLINE")
        );
    }
}

#[test]
fn every_console_action_uses_natural_argv() {
    for (input, executable, tail) in [
        ("cargo build", "/trusted/cargo", vec!["build", "--locked"]),
        ("cargo check", "/trusted/cargo", vec!["check", "--locked"]),
        ("cargo test", "/trusted/cargo", vec!["test", "--locked"]),
        ("cargo run", "/trusted/cargo", vec!["run", "--locked"]),
        (
            "cargo run --release",
            "/trusted/cargo",
            vec!["run", "--release", "--locked"],
        ),
        (
            "cargo clippy",
            "/trusted/cargo-clippy",
            vec!["clippy", "--locked"],
        ),
        ("cargo doc", "/trusted/cargo", vec!["doc", "--locked"]),
        (
            "cargo add serde@1.0.0",
            "/trusted/cargo",
            vec!["add", "serde@1.0.0"],
        ),
        (
            "cargo remove serde",
            "/trusted/cargo",
            vec!["remove", "serde"],
        ),
        ("cargo update", "/trusted/cargo", vec!["update"]),
    ] {
        let request = parse_console_command(input).unwrap();
        let prepared = prepare_console(&request, &tools(), &PathBuf::from("/workspace")).unwrap();
        let mut expected = vec!["run", "pinned", executable];
        expected.extend(tail);
        assert_eq!(
            prepared.command.get_args().collect::<Vec<_>>(),
            expected
                .iter()
                .map(std::ffi::OsStr::new)
                .collect::<Vec<_>>(),
            "{input}"
        );
    }
}

#[test]
fn console_test_forms_prepare_the_exact_literal_tail_after_controller_locking() {
    for input in [
        "cargo test legal_moves",
        "cargo test tests::legal_moves",
        "cargo test -- --nocapture",
        "cargo test -- --no-capture",
        "cargo test -- --show-output",
        "cargo test tests::legal_moves -- --nocapture",
        "cargo test tests::legal_moves -- --no-capture",
        "cargo test tests::legal_moves -- --show-output",
    ] {
        let request = parse_console_command(input).unwrap();
        let prepared = prepare_console(&request, &tools(), &PathBuf::from("/workspace")).unwrap();
        let mut expected = argv(&["run", "pinned", "/trusted/cargo", "test", "--locked"]);
        expected.extend(request.argv[2..].iter().cloned());
        assert_eq!(prepared.command.get_program(), "/trusted/rustup", "{input}");
        assert_eq!(
            prepared.command.get_args().collect::<Vec<_>>(),
            expected
                .iter()
                .map(std::ffi::OsStr::new)
                .collect::<Vec<_>>(),
            "{input}"
        );
    }

    let longest_filter = "a".repeat(256);
    let request = ConsoleCommand {
        action: CargoAction::Test,
        argv: vec!["cargo".into(), "test".into(), longest_filter.clone()],
        stdin: None,
        stdout: None,
    };
    let prepared = prepare_console(&request, &tools(), &PathBuf::from("/workspace")).unwrap();
    assert_eq!(
        prepared.command.get_args().collect::<Vec<_>>(),
        [
            "run",
            "pinned",
            "/trusted/cargo",
            "test",
            "--locked",
            longest_filter.as_str(),
        ]
        .into_iter()
        .map(std::ffi::OsStr::new)
        .collect::<Vec<_>>()
    );
}

#[test]
fn direct_console_test_requests_cannot_bypass_grammar_action_or_routes() {
    let invalid = [
        (
            CargoAction::Test,
            vec!["cargo", "test", "one", "two"],
            None,
            None,
        ),
        (
            CargoAction::Check,
            vec!["cargo", "test", "legal_moves"],
            None,
            None,
        ),
        (CargoAction::Test, vec!["cargo", "test", ""], None, None),
        (
            CargoAction::Test,
            vec!["cargo", "test", "légal_moves"],
            None,
            None,
        ),
        (
            CargoAction::Test,
            vec!["cargo", "test", "legal_moves"],
            Some(WorkspacePath::new("input.bin").unwrap()),
            None,
        ),
        (
            CargoAction::Test,
            vec!["cargo", "test", "legal_moves"],
            None,
            Some(WorkspacePath::new("output.bin").unwrap()),
        ),
    ];
    for (action, arguments, stdin, stdout) in invalid {
        let request = ConsoleCommand {
            action,
            argv: argv(&arguments),
            stdin,
            stdout,
        };
        assert!(
            prepare_console(&request, &tools(), &PathBuf::from("/workspace")).is_err(),
            "accepted direct request {request:?}"
        );
    }

    for filter in ["-leading".to_owned(), "a".repeat(257)] {
        let request = ConsoleCommand {
            action: CargoAction::Test,
            argv: vec!["cargo".into(), "test".into(), filter],
            stdin: None,
            stdout: None,
        };
        assert!(prepare_console(&request, &tools(), &PathBuf::from("/workspace")).is_err());
    }

    assert!(
        prepare(
            CargoAction::Test,
            &argv(&["cargo", "test", "legal_moves"]),
            &tools(),
            &PathBuf::from("/workspace")
        )
        .is_err(),
        "console Test arguments must not broaden instructor commands"
    );
}

#[test]
fn rejection_names_the_complete_student_allowlist_without_internal_ids() {
    let message = parse_console_command("cargo publish")
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "unsupported Cargo command; allowed: cargo build, cargo check, cargo test [FILTER] [-- OUTPUT_OPTION] (OUTPUT_OPTION: --nocapture, --no-capture, or --show-output), cargo run, cargo clippy, cargo doc, cargo add NAME, cargo add NAME@VERSION, cargo remove NAME, cargo update"
    );
    assert!(!message.contains("T10.10"));
    assert!(!message.contains("T4.3"));
}

#[test]
fn unsupported_forms_and_wrong_action_fail_before_any_execution() {
    for args in [
        vec![],
        vec!["sh", "-c", "touch marker"],
        vec!["cargo", "build"],
        vec!["cargo", "+stable", "check"],
        vec!["cargo", "c"],
        vec!["/usr/bin/cargo", "check"],
        vec!["cargo", "check", "--config", "secret"],
        vec!["cargo", "check", "--manifest-path", "../Cargo.toml"],
        vec!["cargo", "check", "--target-dir", "src"],
        vec!["cargo", "check", "--release"],
        vec!["cargo", "check", "--message-format=json"],
        vec!["cargo", "check", "--locked", "--locked"],
        vec!["cargo", "check", "--", "--cfg=unexpected"],
        vec!["cargo", "check", "$(touch marker)"],
        vec!["cargo", "check", "--locked\0"],
    ] {
        assert!(
            prepare(
                CargoAction::Check,
                &argv(&args),
                &tools(),
                &PathBuf::from("/does-not-exist")
            )
            .is_err(),
            "{args:?}"
        );
    }
    assert!(
        prepare(
            CargoAction::Run,
            &argv(&["cargo", "run", "--", "hello"]),
            &tools(),
            &PathBuf::from("/workspace")
        )
        .is_err()
    );
    assert!(
        prepare(
            CargoAction::Format,
            &argv(&["cargo", "fmt", "--", "--config-path", "/tmp/config"]),
            &tools(),
            &PathBuf::from("/workspace")
        )
        .is_err()
    );
}

#[test]
fn missing_optional_tools_and_relative_paths_are_preparation_errors() {
    let mut selected = tools();
    selected.cargo_clippy = None;
    assert!(
        prepare(
            CargoAction::Clippy,
            &argv(&["cargo", "clippy"]),
            &selected,
            &PathBuf::from("/workspace")
        )
        .is_err()
    );
    selected.formatter = None;
    assert!(
        prepare(
            CargoAction::Format,
            &argv(&["cargo", "fmt"]),
            &selected,
            &PathBuf::from("/workspace")
        )
        .is_err()
    );
    selected.rustc = "rustc".into();
    assert!(
        prepare(
            CargoAction::Check,
            &argv(&["cargo", "check"]),
            &selected,
            &PathBuf::from("/workspace")
        )
        .is_err()
    );
}

#[test]
fn child_environment_is_cleared_and_stdin_is_closed() {
    const CHILD: &str = "RUSTRACE_T43_POLICY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let mut selected = tools();
        selected.rustup = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/support/cargo_policy_launcher.sh");
        let mut prepared = prepare(
            CargoAction::Run,
            &argv(&["cargo", "run"]),
            &selected,
            &std::env::temp_dir(),
        )
        .unwrap();
        let summary = format!("{:?}", prepared.summary);
        assert!(!summary.contains("synthetic-secret"));
        assert!(prepared.command.status().unwrap().success());
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "child_environment_is_cleared_and_stdin_is_closed",
            "--nocapture",
        ])
        .env(CHILD, "1");
    child.env("CARGO_HOME", "/synthetic-secret-home");
    for key in [
        "RUSTC",
        "RUSTDOC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTFMT",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_BUILD_RUSTC",
        "CARGO_TARGET_FAKE_RUNNER",
        "CARGO_REGISTRY_TOKEN",
        "RUSTUP_LOG",
        "CLIPPY_ARGS",
        "UNRELATED_SECRET",
    ] {
        child.env(key, "synthetic-secret");
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Real installed tools run only this copied, dependency-free benign package.
/// The repository/instructor workspace is never passed to a prepared command.
#[test]
fn installed_tools_execute_controlled_package_with_network_and_config_overrides() {
    let parent = std::env::temp_dir().join(format!("rustrace-t43-package-{}", std::process::id()));
    std::fs::create_dir(&parent).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(parent.clone());
    // Literal braces in a real workspace path must not become Cargo build-dir
    // templates. Any wrong expansion still stays within our owned parent.
    let root = parent.join("{workspace-root}");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir(root.join(".cargo")).unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cargo-policy");
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "src/lib.rs",
        ".cargo/config.toml",
    ] {
        std::fs::copy(fixture.join(name), root.join(name)).unwrap();
    }
    let pin = std::env::var("RUSTUP_TOOLCHAIN")
        .expect("run contributor tests through installed rustup Cargo");
    let report = rustrace::toolchain::discover(&root, Some(&pin));
    assert!(
        !report.has_blockers(),
        "required installed tools unavailable"
    );
    let rustup = PathBuf::from(&report.probes[0].argv[0]);
    let selection = report.selected_toolchain.unwrap();
    let resolve = |name: &str| {
        let mut command = Command::new(&rustup);
        rustrace::cargo_policy::retain_execution_environment(&mut command);
        let output = command
            .args(["which", "--toolchain", &selection, name])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("RUSTUP_TOOLCHAIN", &selection)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "installed {name} is required by this contributor fixture"
        );
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    };
    let selected = ResolvedTools {
        cargo: resolve("cargo"),
        rustc: resolve("rustc"),
        rustdoc: resolve("rustdoc"),
        cargo_clippy: Some(resolve("cargo-clippy")),
        formatter: Some((resolve("cargo-fmt"), resolve("rustfmt"))),
        rustup,
        selection,
    };
    for (action, subcommand) in [
        (CargoAction::Check, "check"),
        (CargoAction::Test, "test"),
        (CargoAction::Run, "run"),
        (CargoAction::Clippy, "clippy"),
        (CargoAction::Doc, "doc"),
        (CargoAction::Format, "fmt"),
    ] {
        let mut prepared = if action == CargoAction::Run {
            let request = parse_console_command("cargo run").unwrap();
            prepare_console(&request, &selected, &root).unwrap()
        } else {
            prepare(action, &argv(&["cargo", subcommand]), &selected, &root).unwrap()
        };
        let output = prepared.command.output().unwrap();
        assert!(
            output.status.success(),
            "{action:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if action == CargoAction::Run {
            assert_eq!(output.stdout, b"t43-controlled-run-eof\n");
        }
        if action == CargoAction::Test {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("src/lib.rs"),
                "doctest must execute"
            );
        }
    }
    assert!(root.join("target").is_dir());
    assert_eq!(
        std::fs::read_dir(&parent).unwrap().count(),
        1,
        "Cargo expanded the literal workspace path into a different build directory"
    );
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "src/lib.rs",
        ".cargo/config.toml",
    ] {
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            std::fs::read(fixture.join(name)).unwrap(),
            "unexpected fixture mutation at {name}"
        );
    }
}

#[test]
fn locked_compile_failure_never_changes_lockfile_bytes() {
    let parent = std::env::temp_dir().join(format!("rustrace-t10-10-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(parent.join("src")).unwrap();
    std::fs::write(
        parent.join("Cargo.toml"),
        b"[package]\nname='locked-fixture'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nitoa='1'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(parent.join("src/main.rs"), b"fn main() {}\n").unwrap();
    let lock = b"# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"locked-fixture\"\nversion = \"0.1.0\"\n";
    std::fs::write(parent.join("Cargo.lock"), lock).unwrap();

    let pin = std::env::var("RUSTUP_TOOLCHAIN").unwrap();
    let report = rustrace::toolchain::discover(&parent, Some(&pin));
    assert!(
        !report.has_blockers(),
        "required installed tools unavailable"
    );
    let rustup = PathBuf::from(&report.probes[0].argv[0]);
    let selection = report.selected_toolchain.unwrap();
    let resolve = |name: &str| {
        let output = Command::new(&rustup)
            .args(["which", "--toolchain", &selection, name])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .unwrap();
        assert!(output.status.success(), "installed {name} is required");
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    };
    let selected = ResolvedTools {
        cargo: resolve("cargo"),
        rustc: resolve("rustc"),
        rustdoc: resolve("rustdoc"),
        cargo_clippy: None,
        formatter: None,
        rustup,
        selection,
    };
    let output = prepare(
        CargoAction::Check,
        &argv(&["cargo", "check"]),
        &selected,
        &parent,
    )
    .unwrap()
    .command
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(parent.join("Cargo.lock")).unwrap(), lock);
    std::fs::remove_dir_all(parent).unwrap();
}
