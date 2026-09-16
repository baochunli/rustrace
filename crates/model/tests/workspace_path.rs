use std::str::FromStr;

use rustrace_model::{
    MAX_WORKSPACE_COMPONENT_BYTES, MAX_WORKSPACE_PATH_BYTES, MAX_WORKSPACE_PATH_DEPTH,
    WorkspaceDirectory, WorkspaceDirectoryError, WorkspacePath, WorkspacePathError,
};

#[test]
fn accepts_only_canonical_slash_separators_for_the_wire() {
    let path = WorkspacePath::new("src/nested/caf\u{e9}.rs").unwrap();

    assert_eq!(path.as_str(), "src/nested/caf\u{e9}.rs");
    assert_eq!(path.to_string(), "src/nested/caf\u{e9}.rs");
    assert_eq!(
        serde_json::to_string(&path).unwrap(),
        r#""src/nested/café.rs""#
    );
    assert_eq!(
        WorkspacePath::from_str("src/lib.rs").unwrap().as_str(),
        "src/lib.rs"
    );
}

#[test]
fn rejects_backslash_separator_aliases() {
    for input in [
        r"src\lib.rs",
        r"src\\lib.rs",
        r"src\nested/lib.rs",
        r"src/nested\lib.rs",
        r"C:\answer.rs",
        r"\\server\share\answer.rs",
        r"\\?\C:\answer.rs",
    ] {
        assert_eq!(
            WorkspacePath::new(input),
            Err(WorkspacePathError::ContainsBackslash),
            "unexpected result for `{input}`"
        );
    }

    let error = serde_json::from_str::<WorkspacePath>(r#""src\\lib.rs""#).unwrap_err();
    assert!(error.to_string().contains("backslash"), "{error}");
}

#[test]
fn rejects_non_nfc_input_instead_of_selecting_a_different_path() {
    let decomposed = "src/cafe\u{301}.rs";

    assert_eq!(
        WorkspacePath::new(decomposed),
        Err(WorkspacePathError::NotNfc)
    );
    assert!(
        WorkspacePath::new("src/caf\u{e9}.rs").is_ok(),
        "precomposed NFC input remains valid"
    );
    assert!(serde_json::from_str::<WorkspacePath>(r#""src/cafe\u0301.rs""#).is_err());
}

#[test]
fn rejects_rooted_drive_unc_verbatim_and_traversal_paths() {
    for input in [
        "../../outside.rs",
        "src/../../outside.rs",
        "src\\..\\outside.rs",
        "src/..\\outside.rs",
        "/tmp/answer.rs",
        "\\tmp\\answer.rs",
        "C:/answer.rs",
        "C:\\answer.rs",
        "C:answer.rs",
        "c:",
        "//server/share/answer.rs",
        "\\\\server\\share\\answer.rs",
        "\\\\?\\C:\\answer.rs",
        "\\\\.\\C:\\answer.rs",
    ] {
        assert!(WorkspacePath::new(input).is_err(), "accepted `{input}`");
    }
}

#[test]
fn rejects_empty_dot_null_and_empty_components() {
    for input in [
        "",
        ".",
        "..",
        "src/./lib.rs",
        "src\\.\\lib.rs",
        "src//lib.rs",
        "src\\\\lib.rs",
        "src/\\lib.rs",
        "src/lib.rs/",
        "src/lib.rs\\",
        "src/\0lib.rs",
    ] {
        assert!(WorkspacePath::new(input).is_err(), "accepted `{input:?}`");
    }

    assert_eq!(WorkspacePath::new(""), Err(WorkspacePathError::Empty));
    assert_eq!(
        WorkspacePath::new("src/../lib.rs"),
        Err(WorkspacePathError::ParentDirectory { component: 1 })
    );
    assert_eq!(
        WorkspacePath::new("src//lib.rs"),
        Err(WorkspacePathError::EmptyComponent { component: 1 })
    );
}

#[test]
fn enforces_explicit_byte_component_and_depth_limits() {
    let too_long_component = "x".repeat(MAX_WORKSPACE_COMPONENT_BYTES + 1);
    assert!(matches!(
        WorkspacePath::new(&too_long_component),
        Err(WorkspacePathError::ComponentTooLong { .. })
    ));

    let too_deep = std::iter::repeat_n("x", MAX_WORKSPACE_PATH_DEPTH + 1)
        .collect::<Vec<_>>()
        .join("/");
    assert!(matches!(
        WorkspacePath::new(&too_deep),
        Err(WorkspacePathError::TooDeep { .. })
    ));

    let too_many_bytes = format!(
        "{}/{}",
        "a/".repeat(MAX_WORKSPACE_PATH_DEPTH - 1),
        "x".repeat(MAX_WORKSPACE_PATH_BYTES)
    );
    assert!(matches!(
        WorkspacePath::new(&too_many_bytes),
        Err(WorkspacePathError::TooLong { .. } | WorkspacePathError::ComponentTooLong { .. })
    ));

    assert!(WorkspacePath::new("x".repeat(MAX_WORKSPACE_COMPONENT_BYTES)).is_ok());
    let maximum_depth = std::iter::repeat_n("x", MAX_WORKSPACE_PATH_DEPTH)
        .collect::<Vec<_>>()
        .join("/");
    assert!(WorkspacePath::new(maximum_depth).is_ok());
}

#[test]
fn serde_deserialization_cannot_bypass_validation() {
    for json in [
        r#""../outside.rs""#,
        r#""/tmp/answer.rs""#,
        r#""C:answer.rs""#,
        r#""src\\lib.rs""#,
        r#""src//lib.rs""#,
        r#"".""#,
        r#""src/cafe\u0301.rs""#,
    ] {
        assert!(
            serde_json::from_str::<WorkspacePath>(json).is_err(),
            "accepted {json}"
        );
    }
}

#[test]
fn workspace_directory_represents_exact_dot_root_and_relative_paths() {
    let root = WorkspaceDirectory::new(".").unwrap();
    assert!(root.is_root());
    assert_eq!(root.as_str(), ".");
    assert_eq!(root.to_string(), ".");
    assert_eq!(serde_json::to_string(&root).unwrap(), r#"".""#);
    assert_eq!(
        serde_json::from_str::<WorkspaceDirectory>(r#"".""#).unwrap(),
        root
    );

    let relative = WorkspaceDirectory::new("src/nested").unwrap();
    assert!(!relative.is_root());
    assert_eq!(relative.as_str(), "src/nested");
    assert_eq!(serde_json::to_string(&relative).unwrap(), r#""src/nested""#);
}

#[test]
fn workspace_directory_delegates_non_root_validation_and_bounds() {
    for input in [
        "",
        "..",
        "./src",
        "src/.",
        ".\\src",
        "src\\.",
        "/",
        "C:src",
        "src/cafe\u{301}",
    ] {
        assert!(
            WorkspaceDirectory::new(input).is_err(),
            "accepted `{input:?}`"
        );
    }
    assert_eq!(
        WorkspaceDirectory::new("src/.."),
        Err(WorkspaceDirectoryError::Path(
            WorkspacePathError::ParentDirectory { component: 1 }
        ))
    );
    assert_eq!(
        WorkspaceDirectory::new(r"src\nested"),
        Err(WorkspaceDirectoryError::Path(
            WorkspacePathError::ContainsBackslash
        ))
    );

    let maximum = [
        "a".repeat(255),
        "b".repeat(255),
        "c".repeat(255),
        "d".repeat(254),
        "e".to_owned(),
    ]
    .join("/");
    assert_eq!(maximum.len(), MAX_WORKSPACE_PATH_BYTES);
    assert!(WorkspaceDirectory::new(&maximum).is_ok());
    assert!(WorkspaceDirectory::new(format!("{maximum}/x")).is_err());
    assert!(serde_json::from_str::<WorkspaceDirectory>(r#""./src""#).is_err());
    assert!(serde_json::from_str::<WorkspaceDirectory>(r#""src\\nested""#).is_err());
}
