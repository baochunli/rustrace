use super::*;
use rustrace_journal::Journal;
use rustrace_model::{Hash, WorkspacePath};
use rustrace_workspace::rprov_import::{ImportedPackageKind, import_rprov};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MANIFEST: &[u8] = br#"format_version = 1
course_id = "course"
assignment_id = "assignment"
assignment_version = "v1"
title = "Bundle"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["**"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
"#;

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rustrace-{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            base: fs::canonicalize(base).unwrap(),
            workspace: fs::canonicalize(workspace).unwrap(),
        }
    }

    fn started(prefix: &str) -> (Self, ProductionSession) {
        let fixture = Self::new(prefix);
        fs::write(fixture.workspace.join("main.rs"), "A").unwrap();
        let session = ProductionSession::start(&fixture.workspace, MANIFEST).unwrap();
        (fixture, session)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn journal_count(root: &Path, session_id: &rustrace_model::SessionId) -> u64 {
    let path = root.join(".rustrace").join(format!("{session_id}.sqlite"));
    let mut journal = Journal::open_read_only_no_follow(&path).unwrap();
    journal
        .verify_session_chain(session_id)
        .unwrap()
        .event_count
}

fn stored_zip_entry(archive: &[u8], wanted: &str) -> Vec<u8> {
    let mut offset = 0;
    while archive.get(offset..offset + 4) == Some(&0x0403_4b50_u32.to_le_bytes()) {
        let compressed = u32::from_le_bytes(archive[offset + 18..offset + 22].try_into().unwrap());
        let name_len =
            u16::from_le_bytes(archive[offset + 26..offset + 28].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[offset + 28..offset + 30].try_into().unwrap()) as usize;
        let name_start = offset + 30;
        let data_start = name_start + name_len + extra_len;
        let name = std::str::from_utf8(&archive[name_start..name_start + name_len]).unwrap();
        if name == wanted {
            return archive[data_start..data_start + compressed as usize].to_vec();
        }
        offset = data_start + compressed as usize;
    }
    panic!("missing stored ZIP entry {wanted}");
}

fn tamper_stored_entry(mut archive: Vec<u8>, wanted: &str, replacement: &[u8]) -> Vec<u8> {
    let end = archive.len() - 22;
    assert_eq!(&archive[end..end + 4], &0x0605_4b50_u32.to_le_bytes());
    let mut central = u32::from_le_bytes(archive[end + 16..end + 20].try_into().unwrap()) as usize;
    loop {
        assert_eq!(
            &archive[central..central + 4],
            &0x0201_4b50_u32.to_le_bytes()
        );
        let expanded =
            u32::from_le_bytes(archive[central + 24..central + 28].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(archive[central + 28..central + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(archive[central + 30..central + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(archive[central + 32..central + 34].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&archive[central + 46..central + 46 + name_len]).unwrap();
        if name == wanted {
            assert_eq!(replacement.len(), expanded);
            let local = u32::from_le_bytes(archive[central + 42..central + 46].try_into().unwrap())
                as usize;
            let local_name_len =
                u16::from_le_bytes(archive[local + 26..local + 28].try_into().unwrap()) as usize;
            let local_extra_len =
                u16::from_le_bytes(archive[local + 28..local + 30].try_into().unwrap()) as usize;
            let data = local + 30 + local_name_len + local_extra_len;
            archive[data..data + replacement.len()].copy_from_slice(replacement);
            let crc = crc32fast::hash(replacement).to_le_bytes();
            archive[local + 14..local + 18].copy_from_slice(&crc);
            archive[central + 16..central + 20].copy_from_slice(&crc);
            return archive;
        }
        central += 46 + name_len + extra_len + comment_len;
    }
}

#[test]
fn deterministic_retry_uses_only_receipt_bytes_and_imports_both_forms() {
    let (fixture, mut session) = Fixture::started("bundle-deterministic");
    session.execute(EditorCommand::Insert('B')).unwrap();
    let session_id = session.session_id().clone();
    let receipt = session.finalize("student-1").unwrap();
    let terminal_count = journal_count(&fixture.workspace, &session_id);
    let first_path = fixture.base.join("first.zip");
    let second_path = fixture.base.join("second.zip");

    let first = create_bundle(&receipt, &first_path).unwrap();
    fs::write(fixture.workspace.join("main.rs"), "changed live source").unwrap();
    fs::create_dir_all(fixture.workspace.join("target")).unwrap();
    fs::write(fixture.workspace.join("target/debug.bin"), "excluded").unwrap();
    fs::write(fixture.workspace.join("scratch.swp"), "excluded").unwrap();
    fs::create_dir_all(fixture.base.join("test-cases")).unwrap();
    fs::write(fixture.base.join("test-cases/input.txt"), "excluded").unwrap();
    let second = create_bundle(&receipt, &second_path).unwrap();

    let first_bytes = fs::read(&first.path).unwrap();
    let second_bytes = fs::read(&second.path).unwrap();
    assert_eq!(first_bytes, second_bytes);
    assert_eq!(
        first.blake3,
        Hash::from_bytes(*blake3::hash(&first_bytes).as_bytes())
    );
    let first_rprov = stored_zip_entry(&first_bytes, "session.rprov");
    let second_rprov = stored_zip_entry(&second_bytes, "session.rprov");
    assert_eq!(first_rprov, second_rprov);

    let standalone = import_rprov(Cursor::new(&first_rprov)).unwrap();
    assert_eq!(standalone.kind(), ImportedPackageKind::StandaloneRprov);
    let imported = import_rprov(Cursor::new(&first_bytes)).unwrap();
    assert_eq!(imported.kind(), ImportedPackageKind::LmsZip);
    assert_eq!(
        imported.outer_source_files(),
        &[rustrace_workspace::rprov_import::ImportedSourceFile {
            path: WorkspacePath::new("main.rs").unwrap(),
            byte_length: 2,
        }]
    );
    let mut source = Vec::new();
    imported
        .open_outer_source(&WorkspacePath::new("main.rs").unwrap())
        .unwrap()
        .read_to_end(&mut source)
        .unwrap();
    assert_eq!(source, b"BA");
    let outer_hash = hash_imported_outer_source(&imported).unwrap();
    assert_eq!(
        Some(&outer_hash),
        imported.manifest().final_tree_hash.known()
    );
    assert_eq!(
        journal_count(&fixture.workspace, &session_id),
        terminal_count
    );
}

#[test]
fn structurally_valid_modified_outer_source_exposes_hash_mismatch() {
    let (fixture, session) = Fixture::started("bundle-mismatch");
    let receipt = session.finalize("student-1").unwrap();
    let output = create_bundle(&receipt, &fixture.base.join("submission.zip")).unwrap();
    let tampered = tamper_stored_entry(fs::read(output.path).unwrap(), "main.rs", b"Z");
    let imported = import_rprov(Cursor::new(tampered)).unwrap();
    let outer_hash = hash_imported_outer_source(&imported).unwrap();
    assert_ne!(
        Some(&outer_hash),
        imported.manifest().final_tree_hash.known()
    );
}

#[test]
fn existing_destination_is_untouched_and_no_clobber_is_clear() {
    let (fixture, session) = Fixture::started("bundle-no-clobber");
    let receipt = session.finalize("student-1").unwrap();
    let destination = fixture.base.join("existing.zip");
    fs::write(&destination, "keep me").unwrap();

    let error = create_bundle(&receipt, &destination)
        .unwrap_err()
        .to_string();

    assert!(error.contains("already exists"), "{error}");
    assert_eq!(fs::read(&destination).unwrap(), b"keep me");
    assert_eq!(
        fs::read_dir(&fixture.base)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("rustrace-submit"))
            .count(),
        0
    );
}

#[test]
fn replaced_temporary_path_cannot_change_the_validated_installed_inode() {
    let (fixture, first_session) = Fixture::started("bundle-path-replacement-first");
    let first_receipt = first_session.finalize("student-1").unwrap();
    let expected = create_bundle(&first_receipt, &fixture.base.join("expected.zip")).unwrap();

    let (replacement_fixture, replacement_session) =
        Fixture::started("bundle-path-replacement-second");
    let replacement_receipt = replacement_session.finalize("student-2").unwrap();
    let replacement = create_bundle(
        &replacement_receipt,
        &replacement_fixture.base.join("replacement.zip"),
    )
    .unwrap();
    assert_ne!(
        fs::read(&expected.path).unwrap(),
        fs::read(&replacement.path).unwrap()
    );

    let destination = fixture.base.join("attacked.zip");
    match super::bundle::create_bundle_after_replacing_temporary(
        &first_receipt,
        &destination,
        &replacement.path,
    ) {
        Ok(_) => assert_eq!(
            fs::read(&destination).unwrap(),
            fs::read(&expected.path).unwrap(),
            "publication installed bytes from a replaced temporary pathname"
        ),
        Err(error) => {
            assert!(error.to_string().contains("identity"), "{error}");
            assert!(!destination.exists());
        }
    }
}

#[test]
fn submit_refuses_an_incomplete_finalization_capture() {
    let (fixture, session) = Fixture::started("bundle-incomplete-finalization");
    assert!(
        session
            .finalize_with_aggregate_limit("student-1", 0)
            .is_err()
    );
    let destination = fixture.base.join("must-not-exist.zip");

    let error =
        submit_finalized_workspace(&fixture.workspace, "student-1", Some(destination.as_path()))
            .unwrap_err()
            .to_string();

    assert!(error.contains("INCOMPLETE RECOVERY"), "{error}");
    assert!(!destination.exists());
}

#[test]
fn revised_bundle_contains_complete_ancestry_and_only_latest_outer_source() {
    let (fixture, mut parent) = Fixture::started("bundle-revision");
    parent.execute(EditorCommand::Insert('B')).unwrap();
    let parent_receipt = parent.finalize("student-1").unwrap();
    let child_root = fixture.base.join("child");
    fs::create_dir(&child_root).unwrap();
    for (path, bytes) in parent_receipt.final_workspace() {
        let destination = child_root.join(path.as_str());
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(destination, bytes).unwrap();
    }
    let mut child =
        ProductionSession::start_revision(&fixture.workspace, &child_root, MANIFEST).unwrap();
    child.execute(EditorCommand::Insert('C')).unwrap();
    let child_receipt = child.finalize("student-1").unwrap();

    let output = create_bundle(&child_receipt, &fixture.base.join("revision.zip")).unwrap();
    let imported = import_rprov(Cursor::new(fs::read(output.path).unwrap())).unwrap();
    assert_eq!(imported.manifest().segments.len(), 2);
    assert_eq!(imported.outer_source_files().len(), 1);
    let mut source = Vec::new();
    imported
        .open_outer_source(&WorkspacePath::new("main.rs").unwrap())
        .unwrap()
        .read_to_end(&mut source)
        .unwrap();
    assert_eq!(source, b"CBA");
    assert!(
        imported
            .entries()
            .iter()
            .all(|entry| !entry.path.contains("final-workspace"))
    );
    assert!(
        imported
            .entries()
            .iter()
            .all(|entry| !entry.path.ends_with(".zip"))
    );
}
