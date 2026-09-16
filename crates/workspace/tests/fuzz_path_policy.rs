//! Bounded fuzzing of workspace path normalization and assignment path policy.

#[path = "../../../tests/fuzz_support/mod.rs"]
mod fuzz_support;

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use rustrace_model::assignment::{AssignmentManifest, MAX_ALLOWED_PATH_BYTES, MAX_ALLOWED_PATHS};
use rustrace_model::{
    MAX_WORKSPACE_COMPONENT_BYTES, MAX_WORKSPACE_PATH_BYTES, MAX_WORKSPACE_PATH_DEPTH,
    WorkspaceDirectory, WorkspacePath,
};
use rustrace_workspace::{AllowedPathSet, WorkspaceContainmentError, validate_workspace_path};

const PATH_SEED: [u8; 32] = *b"rustrace-path-normalize-seed-v1.";
const POLICY_SEED: [u8; 32] = *b"rustrace-path-policy-fuzz-seedv1";

/// The measured envelope for this target.
///
/// This is **not** a derived ceiling, though these two parsers are the least
/// composite of the six: nothing is sized from a declared count, and the glob
/// set is the only structure that outlives one call.
///
/// The family, one line per input:
///
/// | Input | Peak |
/// | --- | --- |
/// | `patterns-128`, a manifest at `MAX_ALLOWED_PATHS` compiled into a glob set | **115 470 B** |
/// | the randomized `assignment-path-policy` arm at its largest | 54 208 B |
/// | `pattern-bytes-256`, one pattern at `MAX_ALLOWED_PATH_BYTES` | 21 358 B |
/// | `path-1024`, a path at `MAX_WORKSPACE_PATH_BYTES` | 0 B |
/// | `path-1025`, one byte over, rejected | 0 B |
///
/// The bound is a mebibyte, nine times the largest of those.
///
/// **The true worst case over all legal inputs is not derived and may be
/// higher than this.** The bound is an observed envelope guarding against
/// regression; what these cases establish is that normalization and the path
/// policy do not panic and that their allocation is a bounded function of the
/// documented limits.
const ALLOCATION_BOUND: usize = 1024 * MAX_WORKSPACE_PATH_BYTES;
const MUTATION_CEILING: usize = MAX_WORKSPACE_PATH_BYTES + 64;

/// Separators, traversal, aliasing, and encoding tokens that hostile paths use.
///
/// `escape` and `link` name the escaping symlinks [`sandbox_root`] plants, so a
/// generated candidate can actually walk one instead of only ever traversing
/// the real file and directory.
const PATH_TOKENS: &[&str] = &[
    "src", "main.rs", "escape", "link", "etc", "passwd", "..", ".", "/", "//", "\\", "", " ",
    "\u{0}", "\u{7f}", "C:", "~", "CON", ".git", "\u{feff}", "\u{202e}", "\u{212b}", "A\u{30a}",
    "é", "e\u{301}", "İ", "ı", "STRASSE", "straße", "🦀",
];

#[test]
fn fuzz_path_policy() {
    fuzz_support::isolated("fuzz_path_policy", body);
}

fn body() {
    boundary_cases();

    let root = sandbox_root();
    symlink_cases(&root);
    fuzz_support::run_cases(
        "path-normalization",
        PATH_SEED,
        candidate_path(),
        |candidate| {
            let normalized =
                fuzz_support::bounded(ALLOCATION_BOUND, || WorkspacePath::new(&candidate))?;
            let _ =
                fuzz_support::bounded(ALLOCATION_BOUND, || WorkspaceDirectory::new(&candidate))?;
            let Ok(path) = normalized else {
                return Ok(());
            };
            prop_assert!(
                path.as_str().len() <= MAX_WORKSPACE_PATH_BYTES,
                "normalization produced {} bytes",
                path.as_str().len()
            );
            prop_assert!(
                path.components().count() <= MAX_WORKSPACE_PATH_DEPTH,
                "normalization produced {} components",
                path.components().count()
            );
            prop_assert!(
                path.components().all(|component| !component.is_empty()
                    && component != "."
                    && component != ".."),
                "normalization kept a traversal or empty component"
            );
            let renormalized = WorkspacePath::new(path.as_str());
            prop_assert_eq!(
                renormalized.as_ref(),
                Ok(&path),
                "normalization is not idempotent"
            );

            let resolved =
                fuzz_support::bounded(ALLOCATION_BOUND, || validate_workspace_path(&root, &path))?;
            if let Ok(resolved) = resolved {
                prop_assert!(
                    resolved.starts_with(&root),
                    "containment check escaped the workspace root"
                );
            }
            Ok(())
        },
    );

    let manifest = MANIFEST.as_bytes().to_vec();
    let admitted = Cell::new(0_u64);
    let refused = Cell::new(0_u64);
    fuzz_support::run_cases(
        "assignment-path-policy",
        POLICY_SEED,
        (
            // Every mutation of a TOML manifest is likely to stop the parser,
            // so a fifth of the cases carry it through unchanged; otherwise the
            // policy decision below is never reached and its oracle is idle.
            prop_oneof![1 => Just(Vec::new()), 4 => fuzz_support::mutations(6)],
            policy_candidate(),
        ),
        |(operations, candidate)| {
            let bytes = fuzz_support::apply_mutations(&manifest, &operations, MUTATION_CEILING);
            let parsed =
                fuzz_support::bounded(ALLOCATION_BOUND, || AssignmentManifest::parse(&bytes))?;
            let Ok(parsed) = parsed else {
                return Ok(());
            };
            let compiled =
                fuzz_support::bounded(ALLOCATION_BOUND, || AllowedPathSet::from_manifest(&parsed))?;
            let Ok(allowed) = compiled else {
                return Ok(());
            };
            let Ok(path) = WorkspacePath::new(&candidate) else {
                return Ok(());
            };
            let decision = fuzz_support::bounded(ALLOCATION_BOUND, || allowed.validate(&path))?;
            if decision.is_ok() {
                admitted.set(admitted.get() + 1);
            } else {
                refused.set(refused.get() + 1);
            }
            prop_assert_eq!(
                decision.is_ok(),
                matches_any(&parsed.allowed_paths, &path),
                "policy decision for {:?} disagrees with the manifest patterns {:?}",
                path.as_str(),
                parsed.allowed_paths
            );
            Ok(())
        },
    );
    println!(
        "FUZZ_COVERAGE target=assignment-path-policy admitted={} refused={}",
        admitted.get(),
        refused.get()
    );
    assert!(
        admitted.get() > 0 && refused.get() > 0,
        "the policy oracle saw only one decision: {} admitted, {} refused",
        admitted.get(),
        refused.get()
    );
}

/// Paths shaped like a real assignment workspace, so the policy has something
/// it can admit as well as something it must refuse.
const WORKSPACE_TOKENS: &[&str] = &[
    "src",
    "tests",
    "nested",
    "main.rs",
    "lib.rs",
    "mod.rs",
    "Cargo.toml",
    "Cargo.lock",
    "notes.txt",
    "target",
];

fn policy_candidate() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => proptest::collection::vec(0_usize..PATH_TOKENS.len(), 0..8).prop_map(|tokens| join(
            &tokens.iter().map(|index| PATH_TOKENS[*index]).collect::<Vec<_>>()
        )),
        2 => proptest::collection::vec(0_usize..WORKSPACE_TOKENS.len(), 1..4).prop_map(|tokens| {
            join(&tokens.iter().map(|index| WORKSPACE_TOKENS[*index]).collect::<Vec<_>>())
        }),
    ]
}

fn join(components: &[&str]) -> String {
    components.join("/")
}

/// Whether `path` matches any manifest pattern, decided independently of the
/// compiled pattern set the policy uses.
///
/// `AllowedPathSet::from_manifest` normalizes each pattern through
/// `WorkspacePath` and compiles it with `/` treated as a literal separator, and
/// `validate_pattern_syntax` narrows the accepted syntax to literals, `*`
/// within one component, and `**` as a whole component. That is a small enough
/// language to decide here, which is what makes the policy's answer checkable
/// rather than merely non-panicking.
fn matches_any(patterns: &[String], path: &WorkspacePath) -> bool {
    let components = path.components().collect::<Vec<_>>();
    patterns.iter().any(|pattern| {
        WorkspacePath::new(pattern).is_ok_and(|normalized| {
            matches_components(&normalized.components().collect::<Vec<_>>(), &components)
        })
    })
}

fn matches_components(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            (0..=path.len()).any(|skipped| matches_components(rest, &path[skipped..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((component, tail)) if matches_component(head, component) => {
                matches_components(rest, tail)
            }
            _ => false,
        },
    }
}

/// `*` matches any run of characters, never the separator, which never appears
/// inside a component here.
fn matches_component(pattern: &str, component: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return pattern == component;
    };
    if parts.len() == 1 {
        return pattern == component;
    }
    let Some(mut rest) = component.strip_prefix(first) else {
        return false;
    };
    let Some(trimmed) = rest.strip_suffix(last) else {
        return false;
    };
    rest = trimmed;
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(index) => rest = &rest[index + middle.len()..],
            None => return false,
        }
    }
    true
}

/// Fixed inputs at each documented path limit and at the limit plus one.
fn boundary_cases() {
    for length in [
        MAX_WORKSPACE_COMPONENT_BYTES,
        MAX_WORKSPACE_COMPONENT_BYTES + 1,
    ] {
        let candidate = "a".repeat(length);
        let normalized =
            fuzz_support::assert_bounded(&format!("component-{length}"), ALLOCATION_BOUND, || {
                WorkspacePath::new(&candidate)
            });
        assert_eq!(
            normalized.is_ok(),
            length <= MAX_WORKSPACE_COMPONENT_BYTES,
            "component limit is not enforced at {length} bytes"
        );
    }

    for depth in [MAX_WORKSPACE_PATH_DEPTH, MAX_WORKSPACE_PATH_DEPTH + 1] {
        let candidate = vec!["a"; depth].join("/");
        let normalized =
            fuzz_support::assert_bounded(&format!("depth-{depth}"), ALLOCATION_BOUND, || {
                WorkspacePath::new(&candidate)
            });
        assert_eq!(
            normalized.is_ok(),
            depth <= MAX_WORKSPACE_PATH_DEPTH,
            "depth limit is not enforced at {depth} components"
        );
    }

    for length in [MAX_WORKSPACE_PATH_BYTES, MAX_WORKSPACE_PATH_BYTES + 1] {
        let components = length.div_ceil(MAX_WORKSPACE_COMPONENT_BYTES);
        let mut candidate = String::new();
        for index in 0..components {
            if index > 0 {
                candidate.push('/');
            }
            let remaining = length - candidate.len();
            candidate.push_str(&"a".repeat(remaining.min(MAX_WORKSPACE_COMPONENT_BYTES)));
        }
        assert_eq!(candidate.len(), length);
        let label = format!("path-{length}");
        let (_, peak) = fuzz_support::assert_measured(&label, ALLOCATION_BOUND, || {
            WorkspacePath::new(&candidate)
        });
        println!("FUZZ_PEAK case={label} peak={peak}");
    }

    for count in [MAX_ALLOWED_PATHS, MAX_ALLOWED_PATHS + 1] {
        let patterns = (0..count)
            .map(|index| format!("\"src/f{index}.rs\""))
            .collect::<Vec<_>>()
            .join(", ");
        let accepted = compile_patterns(&format!("patterns-{count}"), &patterns);
        assert_eq!(
            accepted,
            count <= MAX_ALLOWED_PATHS,
            "pattern-count limit is not enforced at {count} patterns"
        );
    }

    for length in [MAX_ALLOWED_PATH_BYTES, MAX_ALLOWED_PATH_BYTES + 1] {
        let pattern = format!("src/{}", "a".repeat(length - 4));
        let accepted = compile_patterns(
            &format!("pattern-bytes-{length}"),
            &format!("\"{pattern}\""),
        );
        assert_eq!(
            accepted,
            length <= MAX_ALLOWED_PATH_BYTES,
            "pattern-byte limit is not enforced at {length} bytes"
        );
    }
}

/// Parses a manifest with `patterns` as its `allowed_paths` list and reports
/// whether the whole path policy accepted it.
fn compile_patterns(label: &str, patterns: &str) -> bool {
    let manifest = MANIFEST.replace(
        "allowed_paths = [\"Cargo.toml\", \"src/**/*.rs\"]",
        &format!("allowed_paths = [{patterns}]"),
    );
    let (accepted, peak) = fuzz_support::assert_measured(label, ALLOCATION_BOUND, || {
        AssignmentManifest::parse(manifest.as_bytes())
            .is_ok_and(|parsed| AllowedPathSet::from_manifest(&parsed).is_ok())
    });
    println!("FUZZ_PEAK case={label} peak={peak}");
    accepted
}

/// A workspace directory inside the harness sandbox, holding one real file, one
/// real directory, and two symlinks that escape the root: `escape` at the top
/// and `src/link` one level down, so both a first and a later path component
/// can be a link.
fn sandbox_root() -> PathBuf {
    let root = std::env::temp_dir().join("fuzz-path-workspace");
    fs::create_dir_all(root.join("src")).expect("create sandbox workspace");
    fs::write(root.join("src/main.rs"), b"fn main() {}\n").expect("write sandbox file");
    for (name, target) in [("escape", "/"), ("src/link", "/etc")] {
        let link = root.join(name);
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(target, &link).expect("create escaping symlink");
        }
    }
    root
}

/// Walks each planted symlink and requires containment to refuse it, and walks
/// the real file and requires containment to resolve it under the root.
///
/// The randomized arm can now reach these components too, but this case is what
/// guarantees they are exercised on every run.
fn symlink_cases(root: &Path) {
    for candidate in ["escape", "escape/etc/passwd", "src/link", "src/link/passwd"] {
        let path = WorkspacePath::new(candidate).expect("candidate is a safe relative path");
        let outcome = fuzz_support::assert_bounded(candidate, ALLOCATION_BOUND, || {
            validate_workspace_path(root, &path)
        });
        assert!(
            matches!(
                outcome,
                Err(WorkspaceContainmentError::SymlinkComponent { .. })
            ),
            "containment followed the escaping symlink in {candidate}: {outcome:?}"
        );
    }

    let real = WorkspacePath::new("src/main.rs").unwrap();
    let resolved = fuzz_support::assert_bounded("src/main.rs", ALLOCATION_BOUND, || {
        validate_workspace_path(root, &real)
    })
    .expect("the real workspace file resolves");
    assert!(
        resolved.starts_with(fs::canonicalize(root).expect("canonical sandbox root")),
        "containment resolved a real file outside the workspace root"
    );
}

const MANIFEST: &str = r#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Fuzz"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["Cargo.toml", "src/**/*.rs"]

[commands]
check = ["cargo", "check", "--locked"]
test = ["cargo", "test", "--locked"]
run = ["cargo", "run", "--locked"]
clippy = ["cargo", "clippy", "--locked", "--", "-D", "warnings"]
format = ["cargo", "fmt"]
"#;

fn candidate_path() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => proptest::collection::vec(0_usize..PATH_TOKENS.len(), 0..12).prop_map(|indices| {
            indices
                .iter()
                .map(|index| PATH_TOKENS[*index])
                .collect::<Vec<_>>()
                .join("/")
        }),
        1 => proptest::collection::vec(any::<u8>(), 0..256)
            .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        1 => "\\PC*".prop_map(|value: String| value),
    ]
}
