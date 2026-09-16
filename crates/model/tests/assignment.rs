use rustrace_model::assignment::{
    AssignmentManifest, AssignmentManifestError, MAX_ALLOWED_PATHS, MAX_COMMAND_ARG_BYTES,
    MAX_COMMAND_ARGS, MAX_IDENTIFIER_BYTES, MAX_TITLE_BYTES,
};

const SAMPLE: &str = r#"
format_version = 1
course_id = "ECE1724"
assignment_id = "a3"
assignment_version = "2026-09-01"
title = "Ownership and Graph Traversal"
toolchain = "1.92.0"
edition = "2024"

allowed_paths = [
    "Cargo.toml",
    "Cargo.lock",
    "src/**/*.rs",
    "tests/**/*.rs",
]

[commands]
check = ["cargo", "check", "--locked"]
test = ["cargo", "test", "--locked"]
run = ["cargo", "run", "--locked"]
clippy = ["cargo", "clippy", "--locked", "--", "-D", "warnings"]
format = ["cargo", "fmt"]
"#;

#[test]
fn parses_the_supported_v1_manifest() {
    let manifest = AssignmentManifest::parse(SAMPLE.as_bytes()).expect("valid v1 manifest");

    assert_eq!(manifest.format_version, 1);
    assert_eq!(manifest.course_id, "ECE1724");
    assert_eq!(manifest.assignment_id, "a3");
    assert_eq!(manifest.assignment_version, "2026-09-01");
    assert_eq!(manifest.title, "Ownership and Graph Traversal");
    assert_eq!(manifest.toolchain, "1.92.0");
    assert_eq!(manifest.edition, "2024");
    assert_eq!(manifest.allowed_paths.len(), 4);
    assert_eq!(manifest.commands.check, ["cargo", "check", "--locked"]);
    assert_eq!(manifest.commands.format, ["cargo", "fmt"]);
}

#[test]
fn parses_the_supported_v2_manifest_with_the_v1_fields() {
    let input = SAMPLE.replacen("format_version = 1", "format_version = 2", 1);

    let manifest = AssignmentManifest::parse(input.as_bytes()).expect("valid v2 manifest");

    assert_eq!(manifest.format_version, 2);
    assert_eq!(manifest.course_id, "ECE1724");
    assert_eq!(manifest.assignment_id, "a3");
    assert_eq!(manifest.assignment_version, "2026-09-01");
    assert_eq!(manifest.title, "Ownership and Graph Traversal");
    assert_eq!(manifest.toolchain, "1.92.0");
    assert_eq!(manifest.edition, "2024");
    assert_eq!(manifest.allowed_paths.len(), 4);
    assert_eq!(manifest.commands.check, ["cargo", "check", "--locked"]);
    assert_eq!(manifest.commands.format, ["cargo", "fmt"]);
}

#[test]
fn rejects_unsupported_format_versions_before_using_the_manifest() {
    for version in [0, 3] {
        let input = format!("format_version = {version}\n");
        let error = AssignmentManifest::parse(input.as_bytes()).expect_err("unsupported version");

        assert!(matches!(
            error,
            AssignmentManifestError::UnsupportedVersion {
                found,
                supported: 2
            } if found == version
        ));
        assert!(
            error
                .to_string()
                .contains(&format!("format_version {version}"))
        );
    }
}

#[test]
fn rejects_malformed_and_missing_required_fields_with_context() {
    let malformed = AssignmentManifest::parse(b"format_version = [").expect_err("bad TOML");
    assert!(matches!(
        malformed,
        AssignmentManifestError::Malformed { .. }
    ));
    assert!(malformed.to_string().contains("assignment.toml"));

    let missing = SAMPLE.replace("course_id = \"ECE1724\"\n", "");
    let missing = AssignmentManifest::parse(missing.as_bytes()).expect_err("missing course_id");
    assert!(matches!(missing, AssignmentManifestError::Malformed { .. }));
    assert!(missing.to_string().contains("course_id"));

    let missing_command = SAMPLE.replace("format = [\"cargo\", \"fmt\"]\n", "");
    let missing_command =
        AssignmentManifest::parse(missing_command.as_bytes()).expect_err("missing command");
    assert!(matches!(
        missing_command,
        AssignmentManifestError::Malformed { .. }
    ));
    assert!(missing_command.to_string().contains("format"));
}

#[test]
fn validates_required_text_fields_and_supported_editions() {
    for (field, input) in [
        (
            "course_id",
            SAMPLE.replace("course_id = \"ECE1724\"", "course_id = \" \""),
        ),
        (
            "toolchain",
            SAMPLE.replace("toolchain = \"1.92.0\"", "toolchain = \"\""),
        ),
        (
            "edition",
            SAMPLE.replace("edition = \"2024\"", "edition = \"2027\""),
        ),
    ] {
        let error = AssignmentManifest::parse(input.as_bytes()).expect_err(field);
        assert!(matches!(
            error,
            AssignmentManifestError::InvalidField {
                field: actual,
                ..
            } if actual == field
        ));
    }

    let long_id = "x".repeat(MAX_IDENTIFIER_BYTES + 1);
    let input = SAMPLE.replace(
        "assignment_id = \"a3\"",
        &format!("assignment_id = \"{long_id}\""),
    );
    assert!(matches!(
        AssignmentManifest::parse(input.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "assignment_id",
            ..
        })
    ));

    let long_title = "x".repeat(MAX_TITLE_BYTES + 1);
    let input = SAMPLE.replace(
        "title = \"Ownership and Graph Traversal\"",
        &format!("title = \"{long_title}\""),
    );
    assert!(matches!(
        AssignmentManifest::parse(input.as_bytes()),
        Err(AssignmentManifestError::InvalidField { field: "title", .. })
    ));
}

#[test]
fn allowed_paths_are_required_nonblank_and_bounded_without_matching_them() {
    let empty = SAMPLE.replace(
        "allowed_paths = [\n    \"Cargo.toml\",\n    \"Cargo.lock\",\n    \"src/**/*.rs\",\n    \"tests/**/*.rs\",\n]",
        "allowed_paths = []",
    );
    assert!(matches!(
        AssignmentManifest::parse(empty.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "allowed_paths",
            ..
        })
    ));

    let blank = SAMPLE.replace("\"Cargo.toml\",", "\"   \",");
    assert!(matches!(
        AssignmentManifest::parse(blank.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "allowed_paths",
            ..
        })
    ));

    let paths = (0..=MAX_ALLOWED_PATHS)
        .map(|index| format!("\"path-{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let start = SAMPLE.find("allowed_paths = [").unwrap();
    let commands = SAMPLE.find("\n\n[commands]").unwrap();
    let input = format!(
        "{}allowed_paths = [{paths}]{}",
        &SAMPLE[..start],
        &SAMPLE[commands..]
    );
    assert!(matches!(
        AssignmentManifest::parse(input.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "allowed_paths",
            ..
        })
    ));

    // T1.2 owns path matching; T1.1 only establishes finite, nonblank strings.
    let deferred = SAMPLE.replace("\"Cargo.toml\",", "\"../policy-owned-by-t1.2\",");
    assert!(AssignmentManifest::parse(deferred.as_bytes()).is_ok());
}

#[test]
fn command_argv_is_literal_nonempty_and_bounded() {
    let literal = SAMPLE.replace(
        "check = [\"cargo\", \"check\", \"--locked\"]",
        "check = [\"cargo\", \"; rm -rf ignored-by-no-shell\"]",
    );
    let manifest = AssignmentManifest::parse(literal.as_bytes()).expect("literal argv");
    assert_eq!(
        manifest.commands.check,
        ["cargo", "; rm -rf ignored-by-no-shell"]
    );

    let empty = SAMPLE.replace("check = [\"cargo\", \"check\", \"--locked\"]", "check = []");
    assert!(matches!(
        AssignmentManifest::parse(empty.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "commands.check",
            ..
        })
    ));

    let args = (0..=MAX_COMMAND_ARGS)
        .map(|_| "\"arg\"")
        .collect::<Vec<_>>()
        .join(", ");
    let too_many = SAMPLE.replace(
        "check = [\"cargo\", \"check\", \"--locked\"]",
        &format!("check = [{args}]"),
    );
    assert!(matches!(
        AssignmentManifest::parse(too_many.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "commands.check",
            ..
        })
    ));

    let long_arg = "x".repeat(MAX_COMMAND_ARG_BYTES + 1);
    let too_long = SAMPLE.replace(
        "check = [\"cargo\", \"check\", \"--locked\"]",
        &format!("check = [\"{long_arg}\"]"),
    );
    assert!(matches!(
        AssignmentManifest::parse(too_long.as_bytes()),
        Err(AssignmentManifestError::InvalidField {
            field: "commands.check",
            ..
        })
    ));
}

#[test]
fn assignment_manifest_has_no_unchecked_deserialize_entry_point() {
    trait AmbiguousIfDeserialize<Marker> {
        fn assert_not_deserialize() {}
    }

    impl<T> AmbiguousIfDeserialize<()> for T {}
    impl<T: serde::de::DeserializeOwned> AmbiguousIfDeserialize<u8> for T {}

    <AssignmentManifest as AmbiguousIfDeserialize<_>>::assert_not_deserialize();
}
