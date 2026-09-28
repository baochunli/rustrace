//! Read-only access to outside versions that recovery set aside.
//!
//! Rustrace never adopts file contents it did not record. When it finds that a
//! file changed outside the editor, it keeps the outside version as bounded
//! recovery evidence and restores the recorded contents. This module lists
//! those set-aside versions and shows their contents. It never changes the
//! workspace, the journal, or the evidence.
use super::*;
use std::io::Write;

const USAGE: &str = "Usage: rustrace set-aside WORKSPACE [--show N [--file PATH]]";

/// One outside version that recovery set aside.
#[derive(Clone, Debug)]
pub struct SetAsideVersion {
    /// One-based position in capture order.
    pub number: usize,
    pub evidence_hash: Hash,
    /// Sequence of the first `restore_logical` decision for this evidence.
    pub sequence: u64,
    pub captured: Option<SystemTime>,
    /// Files whose outside contents differed from the recorded contents.
    pub changed_paths: Vec<WorkspacePath>,
    /// The complete outside view captured with the evidence.
    pub outside: Files,
}

impl ProductionSession {
    /// List set-aside versions and the current recorded files, read-only.
    pub fn set_aside_versions(root: &Path) -> Result<(Vec<SetAsideVersion>, Files)> {
        let metadata: SessionMetadata = serde_json::from_slice(&read_initial_metadata(root)?)?;
        let pinned = PinnedWorkspaceRoot::open(root)?;
        let mut owner = pinned
            .open_state_directory()?
            .open_journal_file(&metadata.session_id)?;
        let result = (|| {
            let mut journal = Journal::open_read_only_no_follow(owner.display_path())?;
            let receipt = load_saved_receipt(&owner, &mut journal, &metadata)?;
            let prefix = validate_prefix(&mut journal, &metadata, &receipt, &owner)?;
            let current = prefix.replay.workspace_state().files().clone();
            let versions = collect_versions(&owner, &mut journal, &metadata.session_id)?;
            Ok((versions, current))
        })();
        owner.verify()?;
        owner.release_ownership()?;
        result
    }
}

fn collect_versions(
    owner: &PinnedJournalFile,
    journal: &mut Journal,
    id: &SessionId,
) -> Result<Vec<SetAsideVersion>> {
    let state_directory = owner
        .display_path()
        .parent()
        .ok_or("state directory missing")?
        .to_path_buf();
    let mut versions: Vec<SetAsideVersion> = Vec::new();
    let mut next = 1;
    loop {
        let events = journal.read_events(id, next, MAX_EVENTS_PER_READ)?;
        if events.is_empty() {
            break;
        }
        for envelope in events {
            next = envelope.sequence + 1;
            let Event::RecoveryRecorded(recorded) = &envelope.event else {
                continue;
            };
            if recorded.decision != RecoveryDecision::RestoreLogical
                || versions
                    .iter()
                    .any(|version| version.evidence_hash == recorded.evidence_hash)
            {
                continue;
            }
            let name = format!("evidence-{}.bin", recorded.evidence_hash);
            let bytes = owner.read_artifact(&name, ARTIFACT_LIMIT)?;
            if digest(&bytes) != recorded.evidence_hash {
                return Err("set-aside evidence digest mismatch".into());
            }
            let evidence = read_recovery_evidence(&bytes)?;
            let changed_paths = evidence
                .disk
                .keys()
                .filter(|path| evidence.disk.get(*path) != evidence.logical.get(*path))
                .cloned()
                .collect();
            versions.push(SetAsideVersion {
                number: versions.len() + 1,
                evidence_hash: recorded.evidence_hash,
                sequence: envelope.sequence,
                captured: fs::metadata(state_directory.join(&name))
                    .and_then(|metadata| metadata.modified())
                    .ok(),
                changed_paths,
                outside: evidence.disk,
            });
        }
    }
    Ok(versions)
}

fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
    }
}

#[cfg(unix)]
fn local_minute(time: SystemTime) -> Option<String> {
    let seconds = libc::time_t::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    // SAFETY: `tm` is plain data; localtime_r writes only into it and returns
    // null on failure, which is checked before any field is read.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

#[cfg(not(unix))]
fn local_minute(_time: SystemTime) -> Option<String> {
    None
}

fn safe(value: impl std::fmt::Display) -> String {
    crate::display::label_fmt(format_args!("{value}"), 4096)
}

/// `rustrace set-aside WORKSPACE [--show N [--file PATH]]`
pub fn run_set_aside(args: &[String], output: &mut impl Write) -> Result<()> {
    let mut workspace = None;
    let mut show = None;
    let mut file = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--show" if show.is_none() => show = Some(iter.next().ok_or(USAGE)?.clone()),
            "--file" if file.is_none() => file = Some(iter.next().ok_or(USAGE)?.clone()),
            value if !value.starts_with("--") && workspace.is_none() => {
                workspace = Some(PathBuf::from(value));
            }
            _ => return Err(USAGE.into()),
        }
    }
    let workspace = workspace.ok_or(USAGE)?;
    if file.is_some() && show.is_none() {
        return Err(USAGE.into());
    }
    let safe_root = safe(workspace.display());
    let (versions, current) = ProductionSession::set_aside_versions(&workspace)?;
    if versions.is_empty() {
        writeln!(
            output,
            "No set-aside versions in {safe_root}. Rustrace has not needed to set aside any outside change."
        )?;
        return Ok(());
    }
    if let Some(raw) = show {
        let number = raw
            .parse::<usize>()
            .map_err(|_| format!("version number must be a positive integer; {USAGE}"))?;
        let version = versions
            .iter()
            .find(|version| version.number == number)
            .ok_or_else(|| {
                format!(
                    "there is no set-aside version {number}; run `rustrace set-aside {safe_root}` to list them"
                )
            })?;
        let paths = match &file {
            Some(raw_path) => {
                let path = WorkspacePath::new(raw_path.as_str())?;
                if !version.outside.contains_key(&path) {
                    return Err(format!(
                        "set-aside version {} has no file {}",
                        version.number,
                        safe(raw_path)
                    )
                    .into());
                }
                vec![path]
            }
            None => version.changed_paths.clone(),
        };
        for path in paths {
            let bytes = &version.outside[&path];
            writeln!(
                output,
                "===== set-aside version {} : {} ({} lines) =====",
                version.number,
                safe(&path),
                line_count(bytes)
            )?;
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    for line in text.lines() {
                        writeln!(output, "{}", crate::display::label(line, 4096))?;
                    }
                }
                Err(_) => writeln!(output, "[not UTF-8 text; not shown]")?,
            }
        }
        return Ok(());
    }
    writeln!(
        output,
        "Set-aside outside versions in {safe_root} (oldest first). Nothing was deleted."
    )?;
    for version in &versions {
        let captured = version.captured.and_then(local_minute).map_or_else(
            || "capture time unknown".to_owned(),
            |time| format!("captured {time}"),
        );
        writeln!(
            output,
            "Version {}: {captured}, event {}",
            version.number, version.sequence
        )?;
        for path in &version.changed_paths {
            let outside = &version.outside[path];
            let status = match current.get(path) {
                Some(bytes) if bytes == outside => "same as current".to_owned(),
                Some(bytes) => format!("current has {} lines", line_count(bytes)),
                None => "not in the current workspace".to_owned(),
            };
            writeln!(
                output,
                "  {}: set aside {} lines; {status}",
                safe(path),
                line_count(outside)
            )?;
        }
    }
    writeln!(
        output,
        "Show a version: rustrace set-aside {safe_root} --show N [--file PATH]"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = br#"format_version = 1
course_id = "ECE"
assignment_id = "setaside"
assignment_version = "1"
title = "Set aside"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustrace-set-aside-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("main.rs"), "A").unwrap();
            Self(fs::canonicalize(root).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Everything a read-only command must leave untouched: every workspace
    /// file and every state file, byte for byte. SQLite's `-shm` wal-index is
    /// rewritten by any reader and holds no recorded data, so it is skipped.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, files);
                } else if !path.to_string_lossy().ends_with(".lock")
                    && !path.to_string_lossy().ends_with("-shm")
                {
                    files.insert(path.clone(), fs::read(&path).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(root, &mut files);
        files
    }

    /// Record "BA", then replace main.rs and add helper.rs outside the editor
    /// while Rustrace is closed. Resume sets both aside and restores "BA".
    fn set_aside_outside_work(root: &Path) {
        let mut session = ProductionSession::start(root, MANIFEST).unwrap();
        session.execute(EditorCommand::Insert('B')).unwrap();
        session.save_all().unwrap();
        session.quit().unwrap();
        fs::write(root.join("main.rs"), "fn main() {\n    outside();\n}\n").unwrap();
        fs::write(root.join("helper.rs"), "fn outside() {}\n").unwrap();
        let session = ProductionSession::resume(root, MANIFEST, ResumeChoice::Resume).unwrap();
        assert!(session.external_notice().is_some());
        session.quit().unwrap();
        assert_eq!(fs::read(root.join("main.rs")).unwrap(), b"BA");
        assert!(!root.join("helper.rs").exists());
    }

    fn run(args: &[&str]) -> std::result::Result<String, String> {
        let mut output = Vec::new();
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        run_set_aside(&args, &mut output).map_err(|error| error.to_string())?;
        Ok(String::from_utf8(output).unwrap())
    }

    #[test]
    fn lists_set_aside_versions_with_their_outside_contents() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        set_aside_outside_work(root);
        let main = WorkspacePath::new("main.rs").unwrap();
        let helper = WorkspacePath::new("helper.rs").unwrap();

        let (versions, current) = ProductionSession::set_aside_versions(root).unwrap();
        assert_eq!(versions.len(), 1);
        let version = &versions[0];
        assert_eq!(version.number, 1);
        assert_eq!(version.changed_paths, vec![helper.clone(), main.clone()]);
        assert_eq!(
            version.outside[&main],
            b"fn main() {\n    outside();\n}\n".to_vec()
        );
        assert_eq!(version.outside[&helper], b"fn outside() {}\n".to_vec());
        assert_eq!(current[&main], b"BA".to_vec());
    }

    #[test]
    fn cli_lists_and_shows_without_changing_anything() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        set_aside_outside_work(root);
        let workspace = root.to_str().unwrap();
        let before = snapshot(root);

        let listing = run(&[workspace]).unwrap();
        assert!(listing.contains("Version 1: captured "), "{listing}");
        assert!(
            listing.contains("main.rs: set aside 3 lines; current has 1 lines"),
            "{listing}"
        );
        assert!(
            listing.contains("helper.rs: set aside 1 lines; not in the current workspace"),
            "{listing}"
        );

        let one = run(&[workspace, "--show", "1", "--file", "main.rs"]).unwrap();
        assert!(
            one.contains("set-aside version 1 : main.rs (3 lines)"),
            "{one}"
        );
        assert!(one.contains("    outside();"), "{one}");
        assert!(!one.contains("fn outside() {}"), "{one}");
        let all = run(&[workspace, "--show", "1"]).unwrap();
        assert!(all.contains("fn outside() {}"), "{all}");
        assert!(all.contains("    outside();"), "{all}");

        let after = snapshot(root);
        let changed = before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            changed.is_empty(),
            "listing and showing are read-only: {changed:?}"
        );
        let session = ProductionSession::resume(root, MANIFEST, ResumeChoice::Resume).unwrap();
        assert!(session.external_notice().is_none());
        session.quit().unwrap();
    }

    #[test]
    fn cli_rejects_restore_requests_and_unknown_versions() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        set_aside_outside_work(root);
        let workspace = root.to_str().unwrap();
        for args in [
            vec![workspace, "--restore", "1"],
            vec![workspace, "--file", "main.rs"],
            vec![workspace, "--show"],
        ] {
            assert!(run(&args).unwrap_err().starts_with("Usage:"), "{args:?}");
        }
        assert!(
            run(&[workspace, "--show", "2"])
                .unwrap_err()
                .contains("there is no set-aside version 2")
        );
        assert!(
            run(&[workspace, "--show", "1", "--file", "other.rs"])
                .unwrap_err()
                .contains("has no file other.rs")
        );
    }

    #[test]
    fn a_workspace_without_outside_changes_has_no_versions() {
        let fixture = Fixture::new();
        let session = ProductionSession::start(&fixture.0, MANIFEST).unwrap();
        session.quit().unwrap();
        let listing = run(&[fixture.0.to_str().unwrap()]).unwrap();
        assert!(listing.starts_with("No set-aside versions"), "{listing}");
    }
}
