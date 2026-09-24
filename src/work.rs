//! Assignment selection and the real student command surface.
use crate::display;
use crate::session::{ProductionSession, ReadOnlyFinalizationStatus, ResumeChoice};
use rustrace_model::WorkspacePath;
use rustrace_workspace::{
    RegularFileIdentity,
    assignment_package::{
        ExtractedTestCaseSuite, ExtractionLimits, PublishPreparedWorkspaceError,
        create_empty_directory_no_replace, extract_assignment_package, publish_prepared_workspace,
    },
    create_external_regular_file_in, external_regular_file_exists_in,
    hash::{PinnedWorkspaceRoot, hash_workspace},
    open_external_regular_file_read_in, remove_created_external_regular_file_in,
};
use std::{
    error::Error,
    ffi::OsStr,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn inspection_contents(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => display::label_fmt(format_args!("{text:?}"), 4096),
        Err(_) => display::label_fmt(format_args!("bytes {bytes:02x?}"), 4096),
    }
}

pub fn run_work(args: &[String], output: &mut impl Write) -> Result<()> {
    let package = args.first().ok_or("Usage: rustrace work assignment.rta [--workspace DIR] [--resume|--restore-logical|--inspect|--abandon]")?;
    let package = PathBuf::from(package);
    let mut root = package.with_extension("work");
    let mut choice = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                index += 1;
                root = PathBuf::from(args.get(index).ok_or("--workspace requires a directory")?);
            }
            value @ ("--resume" | "--restore-logical" | "--inspect" | "--abandon")
                if choice.is_none() =>
            {
                choice = Some(value.to_owned());
            }
            _ => return Err("invalid work option or multiple recovery choices".into()),
        }
        index += 1;
    }
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    root =
        fs::canonicalize(parent)?.join(root.file_name().ok_or("workspace must name a directory")?);
    let exists = root.try_exists()?;
    if !exists && choice.is_some() {
        return Err("no existing session at selected workspace".into());
    }
    let validation = unique_sibling(&root, if exists { "validate" } else { "prepare" })?;
    let extracted = extract_package(&package, &validation)?;
    let starter_hash = match hash_workspace(&validation) {
        Ok(hash) => hash,
        Err(error) => {
            let cleanup = fs::remove_dir_all(&validation);
            return Err(cleanup_error(error, cleanup, &validation));
        }
    };
    if let Err(error) =
        PinnedWorkspaceRoot::open(&validation).and_then(|root| root.verify_binding())
    {
        let cleanup = fs::remove_dir_all(&validation);
        return Err(cleanup_error(error, cleanup, &validation));
    }
    if exists {
        fs::remove_dir_all(&validation)?;
    }
    let manifest_bytes = extracted.manifest_bytes.clone();
    let test_case_suite_hash = extracted.test_cases.as_ref().map(|suite| suite.hash);
    let mut incomplete = None;
    let mut selected_metadata = None;
    if exists {
        match ProductionSession::read_metadata(&root) {
            Ok(metadata) => {
                if !ProductionSession::selected_assignment_matches(
                    &root,
                    &metadata,
                    &manifest_bytes,
                    starter_hash,
                    test_case_suite_hash,
                )? {
                    return Err("assignment manifest/starter/test-case-suite identity mismatch; selected session left unchanged".into());
                }
                selected_metadata = Some(metadata);
            }
            Err(error) => {
                let evidence = ProductionSession::inspect_preserved(&root)?;
                crate::session::verify_available_manifest(&evidence, &manifest_bytes)?;
                writeln!(
                    output,
                    "Incomplete startup or invalid session metadata at {}: {}. Original preserved; session identity is unknown. Use --inspect for available evidence or --abandon for linked fresh work; Resume/Restore cannot initialize this original.",
                    display::label_fmt(format_args!("{}", root.display()), 4096),
                    display::label_fmt(format_args!("{error}"), 4096)
                )?;
                incomplete = Some(evidence);
            }
        }
    }
    if exists
        && choice.is_none()
        && incomplete.is_none()
        && matches!(
            ProductionSession::inspect_finalization_read_only(&root)?,
            ReadOnlyFinalizationStatus::Finalized(_)
        )
    {
        return Err(
            "session is finalized and immutable; use `rustrace revise` for a new linked attempt"
                .into(),
        );
    }
    if choice.as_deref() == Some("--inspect") {
        if let Some(evidence) = &incomplete {
            writeln!(
                output,
                "Available preserved evidence (journal not validated):\n{}",
                display::json_preview(evidence)?
            )?;
            return Ok(());
        }
        let inspection = match ProductionSession::inspect(&root) {
            Ok(inspection) => inspection,
            Err(error) => {
                let evidence = ProductionSession::inspect_preserved(&root)?;
                writeln!(
                    output,
                    "Startup/journal validation failed: {}. Saved/logical views are unavailable. Original preserved; use --abandon for linked fresh work. Available evidence:\n{}",
                    display::label_fmt(format_args!("{error}"), 4096),
                    display::json_preview(&evidence)?
                )?;
                return Ok(());
            }
        };
        writeln!(
            output,
            "Validated saved / logical / observed disk views; original journal retained at {}",
            display::label_fmt(format_args!("{}", root.join(".rustrace").display()), 4096)
        )?;
        for (label, files) in [
            ("saved", &inspection.saved),
            ("logical", &inspection.logical),
            ("disk", &inspection.disk),
        ] {
            for (path, bytes) in files {
                writeln!(
                    output,
                    "{label} {} ({} bytes): {}",
                    display::label_fmt(format_args!("{path}"), 4096),
                    bytes.len(),
                    inspection_contents(bytes)
                )?;
            }
        }
        return Ok(());
    }
    crate::update::automatic_check(&root);
    let update_state = crate::update::cached_state();
    if !exists {
        drop(publish_fresh_workspace(
            &validation,
            &root,
            extracted.test_cases.as_ref(),
        )?);
    } else if choice.as_deref() != Some("--abandon")
        && incomplete.is_none()
        && let Some(suite) = extracted.test_cases.as_ref()
    {
        deploy_test_case_suite(&root, suite)?;
    }
    let mut session = if choice.as_deref() == Some("--abandon") {
        let fresh = unique_sibling(&root, "recovery")?;
        let session =
            ProductionSession::abandon_with(&root, &manifest_bytes, test_case_suite_hash, || {
                let fresh_assignment = extract_package(&package, &fresh)?;
                if fresh_assignment.manifest_bytes != manifest_bytes
                    || hash_workspace(&fresh)? != starter_hash
                    || fresh_assignment.test_cases.as_ref().map(|suite| suite.hash)
                        != test_case_suite_hash
                {
                    return Err(
                        "selected package changed during recovery extraction; original preserved"
                            .into(),
                    );
                }
                Ok(fresh.clone())
            })?;
        writeln!(
            output,
            "Original preserved at {}; linked recovery workspace: {} (it starts from the starter code; resuming the original takes it back while this copy records no work)",
            display::label_fmt(format_args!("{}", root.display()), 4096),
            display::label_fmt(format_args!("{}", fresh.display()), 4096)
        )?;
        session
    } else if exists {
        if incomplete.is_some() {
            return Err("incomplete startup cannot Resume/Restore; original preserved. Use --inspect or --abandon to start explicitly linked fresh work".into());
        }
        ProductionSession::resume_selected_assignment(
            &root,
            &manifest_bytes,
            test_case_suite_hash,
            if choice.as_deref() == Some("--restore-logical") {
                ResumeChoice::RestoreLogical
            } else {
                ResumeChoice::Resume
            },
        ).map_err(|error| format!("startup/session cannot resume: {error}; original preserved. Your code and recorded history are intact: fix the cause above and run the same command again. --inspect shows the preserved views; --abandon is a last resort that starts a linked workspace from the starter code"))?
    } else {
        ProductionSession::start_from_assignment(&root, &extracted)?
    };
    let started_from_package = !exists || choice.as_deref() == Some("--abandon");
    let identity_matches = if started_from_package {
        session.metadata().starter_hash == starter_hash
            && session.metadata().test_case_suite_hash == test_case_suite_hash
    } else {
        selected_metadata.as_ref() == Some(session.metadata())
    };
    if !identity_matches {
        session.quit()?;
        return Err("assignment starter identity mismatch; source and provenance preserved".into());
    }
    let report = session.discover_toolchain()?;
    report.write_diagnostics(output)?;
    if report.has_blockers() {
        session.quit()?;
        return Err("required Rust tools unavailable; assignment preserved. Fix the reported tools and use --resume; --inspect remains available without tools".into());
    }
    session.start_language_service()?;
    writeln!(
        output,
        "Rustrace session {}. Elapsed time excludes unknown offline intervals. The Quit menu preserves resumability.",
        session.session_id()
    )?;
    output.flush()?;
    crate::work_editor::run_editor(session, &extracted.manifest.title, update_state)
}

fn preflight_test_case_suite(workspace_root: &Path, suite: &ExtractedTestCaseSuite) -> Result<()> {
    let sibling = test_case_sibling(workspace_root)?;
    let Some(root) = open_existing_test_case_root(&sibling)? else {
        return Ok(());
    };
    for_each_test_case_file(suite, |path, contents| {
        preflight_test_case_file(&root, path, contents)
    })?;
    root.verify_binding()?;
    Ok(())
}

fn deploy_test_case_suite(workspace_root: &Path, suite: &ExtractedTestCaseSuite) -> Result<()> {
    deploy_test_case_suite_with(workspace_root, suite, |_index, _path| Ok(()))
}

fn deploy_test_case_suite_with(
    workspace_root: &Path,
    suite: &ExtractedTestCaseSuite,
    mut before_managed_file: impl FnMut(usize, &WorkspacePath) -> Result<()>,
) -> Result<()> {
    preflight_test_case_suite(workspace_root, suite)?;
    let sibling = test_case_sibling(workspace_root)?;
    let (root, created_root) = match open_existing_test_case_root(&sibling)? {
        Some(root) => (root, false),
        None => (create_empty_directory_no_replace(&sibling)?, true),
    };
    let mut created = Vec::<(WorkspacePath, RegularFileIdentity)>::new();
    let mut managed_file_index = 0;
    let deployment = for_each_test_case_file(suite, |path, contents| {
        managed_file_index += 1;
        before_managed_file(managed_file_index, path)?;
        if external_regular_file_exists_in(&root, path)? {
            return preflight_test_case_file(&root, path, contents);
        }
        let mut file = create_external_regular_file_in(&root, path)?;
        created.push((path.clone(), file.identity()));
        file.file_mut().write_all(contents)?;
        file.file_mut().sync_all()?;
        root.verify_binding()?;
        Ok(())
    })
    .and_then(|()| {
        root.verify_binding()?;
        Ok(())
    });
    if let Err(error) = deployment {
        let mut cleanup_failures = Vec::new();
        for (path, identity) in created.into_iter().rev() {
            if let Err(cleanup) = remove_created_external_regular_file_in(&root, &path, identity) {
                cleanup_failures.push(format!("{path}: {cleanup}"));
            }
        }
        if created_root {
            match root
                .verify_binding()
                .map_err(|error| error.to_string())
                .and_then(|()| fs::remove_dir(&sibling).map_err(|error| error.to_string()))
            {
                Ok(()) => {}
                Err(cleanup) => {
                    cleanup_failures.push(format!("{}: {cleanup}", sibling.display()));
                }
            }
        }
        drop(root);
        let mut message = format!("could not deploy packaged test cases: {error}");
        if !cleanup_failures.is_empty() {
            message.push_str("; cleanup failures: ");
            message.push_str(&cleanup_failures.join("; "));
        }
        return Err(message.into());
    }
    Ok(())
}

fn publish_fresh_workspace(
    prepared: &Path,
    destination: &Path,
    suite: Option<&ExtractedTestCaseSuite>,
) -> Result<PinnedWorkspaceRoot> {
    publish_fresh_workspace_with(prepared, destination, suite, |_index, _path| Ok(()))
}

fn publish_fresh_workspace_with(
    prepared: &Path,
    destination: &Path,
    suite: Option<&ExtractedTestCaseSuite>,
    mut before_managed_file: impl FnMut(usize, &WorkspacePath) -> Result<()>,
) -> Result<PinnedWorkspaceRoot> {
    if let Some(suite) = suite
        && let Err(error) = preflight_test_case_suite(destination, suite)
    {
        let cleanup = fs::remove_dir_all(prepared);
        return Err(cleanup_error(error, cleanup, prepared));
    }
    let published = match publish_prepared_workspace(prepared, destination) {
        Ok(published) => published,
        Err(error) => return Err(cleanup_failed_publication(prepared, error)),
    };
    if let Some(suite) = suite
        && let Err(error) =
            deploy_test_case_suite_with(destination, suite, &mut before_managed_file)
    {
        let cleanup = remove_published_workspace(&published);
        return Err(cleanup_error(error, cleanup, destination));
    }
    Ok(published)
}

fn cleanup_failed_publication(
    prepared: &Path,
    error: PublishPreparedWorkspaceError,
) -> Box<dyn Error> {
    let (cleanup, path) = match error.published_workspace() {
        Some(published) => (
            remove_published_workspace(published),
            published.path().to_owned(),
        ),
        None => (
            fs::remove_dir_all(prepared).map_err(Into::into),
            prepared.to_owned(),
        ),
    };
    cleanup_error(error, cleanup, &path)
}

fn preflight_test_case_file(
    root: &PinnedWorkspaceRoot,
    path: &WorkspacePath,
    expected: &[u8],
) -> Result<()> {
    if !external_regular_file_exists_in(root, path)? {
        return Ok(());
    }
    let mut opened = open_external_regular_file_read_in(root, path)?;
    let limit = u64::try_from(expected.len()).unwrap_or(u64::MAX);
    let mut actual = Vec::with_capacity(expected.len().saturating_add(1));
    opened
        .file_mut()
        .take(limit.saturating_add(1))
        .read_to_end(&mut actual)?;
    root.verify_binding()?;
    if actual != expected {
        return Err(format!(
            "packaged test-case path `{}` already exists with different bytes",
            root.path().join(path.as_str()).display()
        )
        .into());
    }
    Ok(())
}

fn for_each_test_case_file(
    suite: &ExtractedTestCaseSuite,
    mut visit: impl FnMut(&WorkspacePath, &[u8]) -> Result<()>,
) -> Result<()> {
    for case in &suite.cases {
        let input = WorkspacePath::new(format!("{}.in", case.name))?;
        visit(&input, &case.input)?;
        let expected = WorkspacePath::new(format!("{}.expected", case.name))?;
        visit(&expected, &case.expected)?;
    }
    Ok(())
}

fn test_case_sibling(workspace_root: &Path) -> Result<PathBuf> {
    if workspace_root.file_name() == Some(OsStr::new("test-cases")) {
        return Err(
            "the selected workspace cannot be the fixed sibling test-cases directory".into(),
        );
    }
    let parent = workspace_root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or("workspace has no parent for sibling test-cases")?;
    Ok(parent.join("test-cases"))
}

fn open_existing_test_case_root(path: &Path) -> Result<Option<PinnedWorkspaceRoot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "the fixed sibling test-cases root `{}` must not be a symlink",
            path.display()
        )
        .into());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "the fixed sibling test-cases root `{}` must be a real directory",
            path.display()
        )
        .into());
    }
    let root = PinnedWorkspaceRoot::open(path)?;
    if root.path() != path {
        return Err(format!(
            "the fixed sibling test-cases root `{}` is not canonical",
            path.display()
        )
        .into());
    }
    root.verify_binding()?;
    Ok(Some(root))
}

fn cleanup_error(
    primary: impl fmt::Display,
    cleanup: std::result::Result<(), impl fmt::Display>,
    path: &Path,
) -> Box<dyn Error> {
    match cleanup {
        Ok(()) => primary.to_string().into(),
        Err(cleanup) => format!(
            "{primary}; cleanup of attempt-created path `{}` failed: {cleanup}",
            path.display()
        )
        .into(),
    }
}

fn remove_published_workspace(root: &PinnedWorkspaceRoot) -> Result<()> {
    root.verify_binding()?;
    fs::remove_dir_all(root.path())?;
    Ok(())
}

pub(crate) fn unique_sibling(root: &Path, purpose: &str) -> Result<PathBuf> {
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = root
        .file_name()
        .ok_or("workspace must name a directory")?
        .to_string_lossy();
    Ok(parent.join(format!(
        "{name}.{purpose}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )))
}
pub(crate) fn extract_package(
    package: &Path,
    destination: &Path,
) -> Result<rustrace_workspace::assignment_package::ExtractedAssignment> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(package)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > 32 * 1024 * 1024 {
        return Err("assignment package must be a regular file no larger than 32 MiB".into());
    }
    let extracted = extract_assignment_package(&file, destination, ExtractionLimits::default())?;
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(
            "assignment package changed during validation; extracted directory preserved".into(),
        );
    }
    Ok(extracted)
}

/// Thin legacy example entry point; production timing comes from the session clock.
pub fn run_workspace_example() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !(2..=3).contains(&args.len()) {
        return Err(
            "Usage: editor <assignment.toml> <workspace-root> [active-relative-path]".into(),
        );
    }
    let manifest_bytes = fs::read(&args[0])?;
    let manifest = rustrace_model::assignment::AssignmentManifest::parse(&manifest_bytes)?;
    let root = Path::new(&args[1]);
    let mut session = if root.join(".rustrace/session.json").try_exists()? {
        ProductionSession::resume(root, &manifest_bytes, ResumeChoice::Resume)?
    } else {
        ProductionSession::start(root, &manifest_bytes)?
    };
    let report = session.discover_toolchain()?;
    report.write_diagnostics(&mut io::stdout())?;
    if report.has_blockers() {
        session.quit()?;
        return Err("required Rust tools unavailable; workspace and session preserved".into());
    }
    session.start_language_service()?;
    if let Some(path) = args.get(2) {
        session
            .workspace_mut()
            .select_path(&rustrace_model::WorkspacePath::new(
                path.to_str().ok_or("active path must be UTF-8")?,
            )?)?;
        session.workspace_mut().activate_selected()?;
    }
    crate::work_editor::run_editor(session, &manifest.title, crate::update::cached_state())
}

#[cfg(test)]
mod inspection_tests {
    #[test]
    fn source_inspection_bounds_derived_preview_before_formatting() {
        let source = "x".repeat(1024 * 1024);
        let preview = super::inspection_contents(source.as_bytes());
        assert!(preview.len() <= 4096);
        assert!(preview.ends_with("[display truncated]"));
        assert_eq!(source.len(), 1024 * 1024);
    }

    #[test]
    fn invalid_utf8_is_inspected_without_replacement_or_byte_loss() {
        assert_eq!(
            super::inspection_contents(&[0, 0xff, 0xc3]),
            "bytes [00, ff, c3]"
        );
        assert_eq!(super::inspection_contents(b"A\n"), "\"A\\n\"");
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod deployment_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustrace_model::Hash;
    use rustrace_workspace::assignment_package::{
        AssignmentPackageError, ExtractedTestCase, ExtractedTestCaseSuite,
        PublishPreparedWorkspaceError,
    };
    use rustrace_workspace::hash::PinnedWorkspaceRoot;

    use super::{cleanup_failed_publication, publish_fresh_workspace_with};

    #[test]
    fn failed_case_deployment_rolls_back_only_attempt_created_paths() {
        let temp = TempRoot::new();
        let suite = suite();

        let prepared = temp.path().join("absent.prepare");
        let workspace = temp.path().join("absent.work");
        prepare_workspace(&prepared);
        let error =
            publish_fresh_workspace_with(&prepared, &workspace, Some(&suite), fail_managed_file(3))
                .expect_err("third managed file must fail");
        assert!(
            error.to_string().contains("injected deployment failure"),
            "{error}"
        );
        assert!(!prepared.exists());
        assert!(!workspace.exists());
        assert!(!temp.path().join("test-cases").exists());

        let sibling = temp.path().join("test-cases");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("alpha.in"), b"alpha input\n").unwrap();
        fs::write(sibling.join("notes.txt"), b"unrelated\n").unwrap();
        let prepared = temp.path().join("existing.prepare");
        let workspace = temp.path().join("existing.work");
        prepare_workspace(&prepared);
        let error =
            publish_fresh_workspace_with(&prepared, &workspace, Some(&suite), fail_managed_file(3))
                .expect_err("third managed file must fail");
        assert!(
            error.to_string().contains("injected deployment failure"),
            "{error}"
        );
        assert!(!prepared.exists());
        assert!(!workspace.exists());
        assert!(sibling.is_dir());
        assert_eq!(
            fs::read(sibling.join("alpha.in")).unwrap(),
            b"alpha input\n"
        );
        assert_eq!(fs::read(sibling.join("notes.txt")).unwrap(), b"unrelated\n");
        assert!(!sibling.join("alpha.expected").exists());
        assert!(!sibling.join("beta.in").exists());
        assert!(!sibling.join("beta.expected").exists());
    }

    #[test]
    fn publication_failure_cleanup_uses_the_committed_path() {
        let temp = TempRoot::new();
        let prepared = temp.path().join("workspace.prepare");
        let destination = temp.path().join("workspace");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("main.rs"), b"attempt-created\n").unwrap();
        let published = PinnedWorkspaceRoot::open(&destination).unwrap();
        let error = PublishPreparedWorkspaceError::AfterRename {
            source: AssignmentPackageError::InvalidDestination {
                path: destination.clone(),
                reason: "injected post-rename failure".to_owned(),
            },
            published,
        };

        let message = cleanup_failed_publication(&prepared, error).to_string();

        assert!(!destination.exists());
        assert!(
            message.contains("injected post-rename failure"),
            "{message}"
        );
        assert!(
            !message.contains("cleanup of attempt-created path"),
            "{message}"
        );

        fs::create_dir(&prepared).unwrap();
        fs::write(prepared.join("main.rs"), b"prepared\n").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"pre-existing\n").unwrap();
        let error = PublishPreparedWorkspaceError::BeforeRename {
            source: AssignmentPackageError::DestinationExists {
                path: destination.clone(),
            },
        };

        let message = cleanup_failed_publication(&prepared, error).to_string();

        assert!(!prepared.exists());
        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"pre-existing\n"
        );
        assert!(message.contains("already exists"), "{message}");
        assert!(
            !message.contains("cleanup of attempt-created path"),
            "{message}"
        );
    }

    fn fail_managed_file(
        target: usize,
    ) -> impl FnMut(usize, &rustrace_model::WorkspacePath) -> super::Result<()> {
        move |index, _path| {
            if index == target {
                return Err(std::io::Error::other("injected deployment failure").into());
            }
            Ok(())
        }
    }

    fn suite() -> ExtractedTestCaseSuite {
        ExtractedTestCaseSuite {
            cases: vec![
                ExtractedTestCase {
                    name: "alpha".to_owned(),
                    input: b"alpha input\n".to_vec(),
                    expected: b"alpha expected\n".to_vec(),
                },
                ExtractedTestCase {
                    name: "beta".to_owned(),
                    input: b"beta input\n".to_vec(),
                    expected: b"beta expected\n".to_vec(),
                },
            ],
            hash: Hash::zero(),
            total_bytes: 52,
        }
    }

    fn prepare_workspace(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::write(path.join("main.rs"), b"fn main() {}\n").unwrap();
    }

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rustrace-work-deployment-tests-{}-{id}",
                std::process::id()
            ));
            if path.exists() {
                fs::remove_dir_all(&path).unwrap();
            }
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                panic!("failed to remove {}: {error}", self.0.display());
            }
        }
    }
}
