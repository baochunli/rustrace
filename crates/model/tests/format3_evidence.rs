//! Assignment format 3 evidence: program arguments, closed-stdin packaged
//! cases, fixture working directories, and the comparison invocation block.
use rustrace_model::{
    ControlledCommandStarted, DecodeOutcome, DecodePolicy, Event, MAX_TEST_CASE_ARG_BYTES,
    MAX_TEST_CASE_ARGS, MAX_TEST_CASE_ARGS_FILE_BYTES, TestCaseArgsError, decode_envelope,
    encode_envelope, parse_test_case_args, test_case_args_blake3,
};
use serde_json::{Value, json};

fn envelope(event: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "format_version": 1, "session_id": "runner", "sequence": 3,
        "monotonic_millis": 12, "wall_clock_utc": null,
        "previous_event_hash": "00".repeat(32), "event_hash": "00".repeat(32),
        "event": event
    }))
    .unwrap()
}

fn start(action: &str, subcommand: &str, tail: &[&str], route: Option<Value>) -> Vec<u8> {
    let mut argv = vec![
        json!("/trusted/rustup"),
        json!("run"),
        json!("fixture"),
        json!("/trusted/cargo"),
        json!(subcommand),
    ];
    argv.extend(tail.iter().map(|argument| json!(argument)));
    let mut payload = json!({
        "command_id":"command-2", "action":action, "argv":argv,
        "environment":{"policy_version":1,"retained_names":["HOME","PATH"]},
        "selected_toolchain":"fixture",
        "tools":[
            {"component":"rustup","executable":"/trusted/rustup","version":"rustup fixture"},
            {"component":"rustc","executable":"/trusted/rustc","version":"rustc fixture"},
            {"component":"cargo","executable":"/trusted/cargo","version":"cargo fixture"},
            {"component":"rustdoc","executable":"/trusted/rustdoc","version":"rustdoc fixture"}
        ],
        "before":{"checkpoint_sequence":2,"checkpoint_event_hash":"00".repeat(32),
            "workspace_hash":"00".repeat(32),"workspace_version":1},
        "deadline_millis":300000,"output_limit":8388608
    });
    if let Some(route) = route {
        payload["console"] = route;
    }
    envelope(json!({"type":"controlled_command_started","payload":payload}))
}

fn run(tail: &[&str], route: Value) -> Vec<u8> {
    start("run", "run", tail, Some(route))
}

fn accepted(bytes: &[u8]) -> bool {
    decode_envelope(bytes, DecodePolicy::RejectUnsupported).is_ok()
}

fn decoded_start(bytes: &[u8]) -> ControlledCommandStarted {
    let DecodeOutcome::Decoded(envelope) =
        decode_envelope(bytes, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("start was skipped")
    };
    let Event::ControlledCommandStarted(start) = envelope.event else {
        panic!("not a start")
    };
    start
}

const FIXTURES: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const MANIFEST_PATH: &str = "../../lab2.work/Cargo.toml";

fn fixtures() -> Value {
    json!({"kind":"fixtures","fixtures_blake3":FIXTURES})
}

#[test]
fn args_files_parse_one_literal_lf_terminated_argument_per_line() {
    assert_eq!(parse_test_case_args(b"").unwrap(), Vec::<String>::new());
    assert_eq!(
        parse_test_case_args(b"-i\nhello world\n  spaced  \n--\n").unwrap(),
        ["-i", "hello world", "  spaced  ", "--"]
    );
    assert_eq!(
        parse_test_case_args("caf\u{e9}\n'quoted'\n$HOME\n*.txt\n".as_bytes()).unwrap(),
        ["caf\u{e9}", "'quoted'", "$HOME", "*.txt"]
    );

    for (bytes, error) in [
        (&b"one"[..], TestCaseArgsError::MissingFinalNewline),
        (&b"one\ntwo"[..], TestCaseArgsError::MissingFinalNewline),
        (&b"\n"[..], TestCaseArgsError::EmptyArgument { line: 1 }),
        (
            &b"one\n\n"[..],
            TestCaseArgsError::EmptyArgument { line: 2 },
        ),
        (
            &b"one\r\n"[..],
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            &b"a\tb\n"[..],
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (
            &b"ok\nnul\0\n"[..],
            TestCaseArgsError::ControlCharacter { line: 2 },
        ),
        (
            &b"\x1b[31m\n"[..],
            TestCaseArgsError::ControlCharacter { line: 1 },
        ),
        (&b"\xff\n"[..], TestCaseArgsError::InvalidUtf8),
    ] {
        assert_eq!(parse_test_case_args(bytes), Err(error), "{bytes:?}");
    }
    assert_eq!(
        parse_test_case_args("\u{85}\n".as_bytes()),
        Err(TestCaseArgsError::ControlCharacter { line: 1 }),
        "C1 controls are control characters too"
    );
}

#[test]
fn args_files_enforce_count_argument_and_file_bounds_at_limit_plus_one() {
    let maximum_count = "a\n".repeat(MAX_TEST_CASE_ARGS);
    assert_eq!(
        parse_test_case_args(maximum_count.as_bytes())
            .unwrap()
            .len(),
        MAX_TEST_CASE_ARGS
    );
    let over_count = "a\n".repeat(MAX_TEST_CASE_ARGS + 1);
    assert_eq!(
        parse_test_case_args(over_count.as_bytes()),
        Err(TestCaseArgsError::TooManyArguments)
    );

    let longest = format!("{}\n", "x".repeat(MAX_TEST_CASE_ARG_BYTES));
    assert_eq!(
        parse_test_case_args(longest.as_bytes()).unwrap()[0].len(),
        1024
    );
    let too_long = format!("{}\n", "x".repeat(MAX_TEST_CASE_ARG_BYTES + 1));
    assert_eq!(
        parse_test_case_args(too_long.as_bytes()),
        Err(TestCaseArgsError::ArgumentTooLong { line: 1 })
    );

    // Eight maximum arguments fill the file bound exactly with their LFs.
    let mut exact = format!("{}\n", "x".repeat(MAX_TEST_CASE_ARG_BYTES - 1)).repeat(8);
    assert_eq!(exact.len(), MAX_TEST_CASE_ARGS_FILE_BYTES);
    assert_eq!(parse_test_case_args(exact.as_bytes()).unwrap().len(), 8);
    exact.push_str("y\n");
    assert_eq!(
        parse_test_case_args(exact.as_bytes()),
        Err(TestCaseArgsError::TooLarge {
            actual: MAX_TEST_CASE_ARGS_FILE_BYTES + 2
        })
    );
}

#[test]
fn argument_list_hash_is_golden_and_keeps_argument_boundaries() {
    // The documented encoding, built independently of the model helper.
    let mut material = b"rustrace.test-case-args.v1".to_vec();
    material.extend_from_slice(&3_u32.to_be_bytes());
    for argument in ["-n", "fn main", "src/lib.rs"] {
        material.extend_from_slice(&(argument.len() as u32).to_be_bytes());
        material.extend_from_slice(argument.as_bytes());
    }
    assert_eq!(
        test_case_args_blake3(&["-n", "fn main", "src/lib.rs"]).to_string(),
        blake3::hash(&material).to_hex().as_str()
    );
    assert_eq!(
        test_case_args_blake3::<&str>(&[]).to_string(),
        "9a46af909f6af44b151b1b755c5fbc909f8d93a48625dd4941c3f858c198c115"
    );
    assert_eq!(
        test_case_args_blake3(&["-n", "fn main", "src/lib.rs"]).to_string(),
        "dfb93a2b8badc09da3e13be0965b9c1dfff9b820f1aa309c251e773a2fe9997f"
    );
    assert_ne!(
        test_case_args_blake3(&["a b"]),
        test_case_args_blake3(&["a", "b"])
    );
    assert_ne!(
        test_case_args_blake3(&["ab"]),
        test_case_args_blake3(&["a", "b"])
    );
    assert_ne!(
        test_case_args_blake3(&["a"]),
        test_case_args_blake3::<&str>(&[])
    );
}

#[test]
fn format_three_routes_have_exact_canonical_json() {
    let route = json!({
        "stdin":{"kind":"closed"},"stdout":{"kind":"console"},
        "args":["-n","fn main","--"],
        "working_directory":{"kind":"fixtures","fixtures_blake3":FIXTURES},
        "test_case":"search-1"
    });
    let bytes = run(
        &[
            "--locked",
            "--manifest-path",
            MANIFEST_PATH,
            "--",
            "-n",
            "fn main",
            "--",
        ],
        route.clone(),
    );
    let DecodeOutcome::Decoded(decoded) =
        decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap()
    else {
        panic!("format 3 start was skipped")
    };
    let encoded = encode_envelope(&decoded).unwrap();
    let value: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value["event"]["payload"]["console"], route);
    let text = String::from_utf8(encoded).unwrap();
    assert!(
        text.contains(concat!(
            r#""console":{"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"#,
            r#""args":["-n","fn main","--"],"#,
            r#""working_directory":{"kind":"fixtures","fixtures_blake3":"4444"#
        )),
        "{text}"
    );
    assert!(text.contains(r#""test_case":"search-1"}"#), "{text}");

    // Explicit defaults decode but never re-encode.
    let explicit = json!({
        "stdin":{"kind":"submitted"},"stdout":{"kind":"console"},
        "args":[],"working_directory":{"kind":"workspace"},"test_case":null
    });
    let start = decoded_start(&run(&["--locked"], explicit));
    let value = serde_json::to_value(start.console.unwrap()).unwrap();
    assert_eq!(
        value,
        json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"}})
    );
}

#[test]
fn run_argv_tail_must_equal_the_route_arguments_after_one_separator() {
    let route =
        |args: Value| json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},"args":args});
    for (tail, args) in [
        (&["--locked", "--", "a"][..], json!(["a"])),
        (
            &["--release", "--locked", "--", "a", "b c"][..],
            json!(["a", "b c"]),
        ),
        (
            &["--locked", "--", "--", "--locked"][..],
            json!(["--", "--locked"]),
        ),
        (
            &["--locked", "--", "--message-format=json"][..],
            json!(["--message-format=json"]),
        ),
    ] {
        assert!(accepted(&run(tail, route(args.clone()))), "{tail:?} {args}");
    }
    for (tail, args) in [
        // Missing, extra, reordered, or duplicated arguments.
        (&["--locked"][..], json!(["a"])),
        (&["--locked", "--"][..], json!(["a"])),
        (&["--locked", "--", "a", "b"][..], json!(["a"])),
        (&["--locked", "--", "b", "a"][..], json!(["a", "b"])),
        (&["--locked", "--", "a"][..], json!([])),
        (&["--locked", "--"][..], json!([])),
        // The separator is required, in its one position.
        (&["--locked", "a"][..], json!(["a"])),
        (&["--", "a", "--locked"][..], json!(["a"])),
        (&["--locked", "--release", "--", "a"][..], json!(["a"])),
        // Historical and structured forms never carry arguments.
        (&["--frozen", "--", "a"][..], json!(["a"])),
        (&["--frozen"][..], json!(["a"])),
        (
            &["--message-format=json", "--locked", "--", "a"][..],
            json!(["a"]),
        ),
        (&["--message-format=json", "--locked"][..], json!(["a"])),
        // Argument grammar.
        (&["--locked", "--", ""][..], json!([""])),
        (&["--locked", "--", "a\tb"][..], json!(["a\tb"])),
        (&["--locked", "--", "a\rb"][..], json!(["a\rb"])),
    ] {
        assert!(
            !accepted(&run(tail, route(args.clone()))),
            "{tail:?} {args}"
        );
    }
}

#[test]
fn run_arguments_are_bounded_like_packaged_args_files() {
    let separated = |args: &[String]| {
        let mut tail = vec!["--locked".to_owned(), "--".to_owned()];
        tail.extend(args.iter().cloned());
        let tail = tail.iter().map(String::as_str).collect::<Vec<_>>();
        run(
            &tail,
            json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},"args":args}),
        )
    };
    let maximum = vec!["a".to_owned(); MAX_TEST_CASE_ARGS];
    assert!(
        accepted(&separated(&maximum)),
        "argv beyond 40 with 64 arguments"
    );
    let over = vec!["a".to_owned(); MAX_TEST_CASE_ARGS + 1];
    assert!(!accepted(&separated(&over)));
    let longest = vec!["x".repeat(MAX_TEST_CASE_ARG_BYTES)];
    assert!(accepted(&separated(&longest)));
    let too_long = vec!["x".repeat(MAX_TEST_CASE_ARG_BYTES + 1)];
    assert!(!accepted(&separated(&too_long)));
    let total = vec!["x".repeat(MAX_TEST_CASE_ARG_BYTES); 8];
    assert!(accepted(&separated(&total)));
    let mut over_total = total.clone();
    over_total.push("y".to_owned());
    assert!(!accepted(&separated(&over_total)));
}

#[test]
fn fixture_runs_record_one_relative_manifest_path_in_a_fixed_slot() {
    let route = |args: Value| {
        json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},
            "args":args,"working_directory":fixtures()})
    };
    // `STEM.test-cases` must fit in 255 bytes, where STEM drops a `.work`.
    let longest = format!("../../{}/Cargo.toml", "n".repeat(244));
    let longest_work = format!("../../{}.work/Cargo.toml", "n".repeat(244));
    let too_long = format!("../../{}/Cargo.toml", "n".repeat(245));
    let too_long_work = format!("../../{}.work/Cargo.toml", "n".repeat(245));
    for (tail, args) in [
        (
            &["--locked", "--manifest-path", MANIFEST_PATH][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../Lab 2 (final).work/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../lab2/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", longest.as_str()][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", longest_work.as_str()][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../test-cases.work/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../.WORK/Cargo.toml"][..],
            json!([]),
        ),
        (
            &[
                "--release",
                "--locked",
                "--manifest-path",
                MANIFEST_PATH,
                "--",
                "x",
            ][..],
            json!(["x"]),
        ),
    ] {
        assert!(accepted(&run(tail, route(args.clone()))), "{tail:?}");
    }
    for (tail, args) in [
        (&["--locked"][..], json!([])),
        (&["--locked", "--", "x"][..], json!(["x"])),
        (&["--locked", "--manifest-path"][..], json!([])),
        (
            &["--locked", "--manifest-path", "Cargo.toml"][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "/home/student/lab2.work/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../lab2.work/Cargo.toml"][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../../lab2.work/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../course/lab2.work/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../../Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../.././Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../..//Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../lab2.work/Cargo.lock"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../lab2.work/Cargo.toml/"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "./../lab2.work/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../a\nb/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", too_long.as_str()][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", too_long_work.as_str()][..],
            json!([]),
        ),
        // Names Rustrace never gives a format 3 case folder.
        (
            &["--locked", "--manifest-path", "../../lab{2}.work/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../lab}2/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../test-cases/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../Test-Cases/Cargo.toml"][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../lab2.test-cases/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                "../../lab2.TEST-CASES/Cargo.toml",
            ][..],
            json!([]),
        ),
        (
            &["--locked", "--manifest-path", "../../.work/Cargo.toml"][..],
            json!([]),
        ),
        (
            &["--manifest-path", MANIFEST_PATH, "--locked"][..],
            json!([]),
        ),
        (
            &["--locked", "--", "x", "--manifest-path", MANIFEST_PATH][..],
            json!(["x"]),
        ),
        (
            &[
                "--locked",
                "--manifest-path",
                MANIFEST_PATH,
                "--manifest-path",
                MANIFEST_PATH,
            ][..],
            json!([]),
        ),
        (&["--frozen"][..], json!([])),
    ] {
        assert!(!accepted(&run(tail, route(args.clone()))), "{tail:?}");
    }

    let workspace = json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"}});
    assert!(!accepted(&run(
        &["--locked", "--manifest-path", MANIFEST_PATH],
        workspace
    )));
    let malformed = json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},
        "working_directory":{"kind":"fixtures"}});
    assert!(!accepted(&run(
        &["--locked", "--manifest-path", MANIFEST_PATH],
        malformed
    )));
}

#[test]
fn closed_stdin_run_requires_a_packaged_test_case_marker() {
    let closed = json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}});
    assert!(
        !accepted(&run(&["--locked"], closed)),
        "a console Run without a packaged case never has closed stdin"
    );
    for route in [
        json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"test_case":"no-input"}),
        json!({"stdin":{"kind":"file","path":"with-input.in"},"stdout":{"kind":"console"},
            "test_case":"with-input"}),
        json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"test_case":"a",
            "args":["x"],"working_directory":fixtures()}),
    ] {
        let tail: &[&str] = if route.get("args").is_some() {
            &["--locked", "--manifest-path", MANIFEST_PATH, "--", "x"]
        } else {
            &["--locked"]
        };
        assert!(accepted(&run(tail, route.clone())), "{route}");
    }
    for route in [
        json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},"test_case":"a"}),
        json!({"stdin":{"kind":"file","path":"b.in"},"stdout":{"kind":"console"},
            "test_case":"a"}),
        json!({"stdin":{"kind":"file","path":"nested/a.in"},"stdout":{"kind":"console"},
            "test_case":"a"}),
        json!({"stdin":{"kind":"file","path":"a.expected"},"stdout":{"kind":"console"},
            "test_case":"a"}),
        json!({"stdin":{"kind":"closed"},"stdout":{"kind":"file","path":"a.out"},
            "test_case":"a"}),
        json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"test_case":"bad.name"}),
        json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"test_case":""}),
    ] {
        assert!(!accepted(&run(&["--locked"], route.clone())), "{route}");
    }
    let marked = json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"},"test_case":"a"});
    assert!(!accepted(&run(&["--frozen"], marked.clone())));
    assert!(!accepted(&run(
        &["--message-format=json", "--locked"],
        marked
    )));
}

#[test]
fn only_console_runs_carry_arguments_fixtures_or_cases() {
    for addition in [
        json!({"args":["x"]}),
        json!({"working_directory":fixtures()}),
        json!({"test_case":"a"}),
    ] {
        let mut route = json!({"stdin":{"kind":"closed"},"stdout":{"kind":"console"}});
        for (key, value) in addition.as_object().unwrap() {
            route[key] = value.clone();
        }
        for (action, subcommand, tail) in [
            ("check", "check", &["--locked"][..]),
            ("test", "test", &["--locked"][..]),
            ("build", "build", &["--locked"][..]),
            ("doc", "doc", &["--locked"][..]),
            ("update", "update", &[][..]),
        ] {
            assert!(
                !accepted(&start(action, subcommand, tail, Some(route.clone()))),
                "{action} {route}"
            );
        }
    }
}

#[test]
fn cargo_argv_excludes_only_the_recorded_program_arguments() {
    let start = decoded_start(&run(
        &["--locked", "--", "--message-format=json"],
        json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"},
            "args":["--message-format=json"]}),
    ));
    assert_eq!(start.argv.len(), 8);
    assert_eq!(
        start.cargo_argv(),
        [
            "/trusted/rustup",
            "run",
            "fixture",
            "/trusted/cargo",
            "run",
            "--locked"
        ]
    );
    let plain = decoded_start(&run(
        &["--locked"],
        json!({"stdin":{"kind":"submitted"},"stdout":{"kind":"console"}}),
    ));
    assert_eq!(plain.cargo_argv(), plain.argv);
}

#[test]
fn format_three_comparisons_have_exact_golden_json() {
    let expected = "22".repeat(32);
    let args = test_case_args_blake3(&["-n", "main"]).to_string();
    let input = "55".repeat(32);
    let events = [
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"search_1",
            "expected_blake3":expected,"actual_blake3":expected,
            "outcome":{"kind":"pass"},
            "invocation":{"args_blake3":args,"stdin":{"kind":"closed"},
                "fixtures_blake3":FIXTURES}
        }}),
        json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"search_1",
            "expected_blake3":expected,"actual_blake3":null,
            "outcome":{"kind":"error","reason":"nonzero_exit"},
            "invocation":{"args_blake3":args,"stdin":{"kind":"file","blake3":input}}
        }}),
    ];
    for event in events {
        let DecodeOutcome::Decoded(decoded) =
            decode_envelope(&envelope(event.clone()), DecodePolicy::RejectUnsupported).unwrap()
        else {
            panic!("comparison was skipped")
        };
        let encoded: Value = serde_json::from_slice(&encode_envelope(&decoded).unwrap()).unwrap();
        assert_eq!(encoded["event"], event);
    }

    for invocation in [
        json!({"stdin":{"kind":"closed"}}),
        json!({"args_blake3":args}),
        json!({"args_blake3":args,"stdin":{"kind":"file"}}),
        json!({"args_blake3":args,"stdin":{"kind":"submitted"}}),
        json!({"args_blake3":args,"stdin":{"kind":"closed"},"surplus":true}),
        json!({"args_blake3":"00","stdin":{"kind":"closed"}}),
    ] {
        let event = json!({"type":"test_case_compared","payload":{
            "command_id":"command-2","case":"search_1",
            "expected_blake3":expected,"actual_blake3":expected,
            "outcome":{"kind":"pass"},"invocation":invocation
        }});
        assert!(
            decode_envelope(&envelope(event.clone()), DecodePolicy::RejectUnsupported).is_err(),
            "accepted {event}"
        );
    }
}

#[test]
fn packaged_case_constructors_build_evidence_that_validates_and_links() {
    use rustrace_model::{
        ConsoleCommandRoute, ConsoleStdinRoute, ConsoleWorkingDirectory, Hash, TestCaseInvocation,
        TestCaseStdin,
    };

    let fixtures = Hash::from_bytes([0x44; Hash::LENGTH]);
    let route = ConsoleCommandRoute::packaged_case(
        "search-1",
        false,
        vec!["-n".to_owned(), "fn main".to_owned()],
        Some(fixtures),
    )
    .unwrap();
    assert_eq!(route.stdin, ConsoleStdinRoute::Closed);
    assert_eq!(
        route.working_directory,
        ConsoleWorkingDirectory::Fixtures {
            fixtures_blake3: fixtures
        }
    );
    let bytes = run(
        &[
            "--locked",
            "--manifest-path",
            MANIFEST_PATH,
            "--",
            "-n",
            "fn main",
        ],
        serde_json::to_value(&route).unwrap(),
    );
    assert!(accepted(&bytes));

    let invocation = TestCaseInvocation::for_route(&route, None).unwrap();
    assert_eq!(
        invocation.args_blake3,
        test_case_args_blake3(&["-n", "fn main"])
    );
    assert_eq!(invocation.stdin, TestCaseStdin::Closed);
    assert_eq!(invocation.fixtures_blake3, Some(fixtures));
    assert!(TestCaseInvocation::for_route(&route, Some(fixtures)).is_none());

    let with_input = ConsoleCommandRoute::packaged_case("case", true, Vec::new(), None).unwrap();
    assert_eq!(
        serde_json::to_value(&with_input).unwrap(),
        json!({"stdin":{"kind":"file","path":"case.in"},"stdout":{"kind":"console"},
            "test_case":"case"})
    );
    let input = Hash::from_bytes([0x55; Hash::LENGTH]);
    assert_eq!(
        TestCaseInvocation::for_route(&with_input, Some(input))
            .unwrap()
            .stdin,
        TestCaseStdin::File { blake3: input }
    );
    assert!(TestCaseInvocation::for_route(&with_input, None).is_none());
    let unmarked = ConsoleCommandRoute::new(
        ConsoleStdinRoute::Submitted,
        rustrace_model::ConsoleStdoutRoute::Console,
    );
    assert!(TestCaseInvocation::for_route(&unmarked, None).is_none());

    assert!(ConsoleCommandRoute::packaged_case("bad.name", true, Vec::new(), None).is_err());
    assert!(ConsoleCommandRoute::packaged_case("case", true, vec![String::new()], None).is_err());
}
