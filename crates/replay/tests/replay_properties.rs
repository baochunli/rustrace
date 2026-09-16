use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{
        Config, FileFailurePersistence, RngAlgorithm, TestCaseError, TestCaseResult, TestRng,
        TestRunner,
    },
};
use rustrace_editor::{EditorBuffer, EditorEffects, EditorTransaction, SelectionState};
use rustrace_journal::{CheckpointFile, CheckpointSnapshot, OpenDocument, StoredCheckpoint};
use rustrace_model::{
    DocumentId, EditOrigin, Event, EventEnvelope, FORMAT_VERSION_V1, FileCreated, FileDeleted,
    FileFocused, FileRenamed, Hash, SelectionChanged, SessionId, SubmissionFinalized, TextEdit,
    WorkspacePath, document_hash,
};
use rustrace_replay::{ReplayEngine, ReplayError};
use rustrace_workspace::hash::hash_entries;

// Primitive seeds shrink well; CaseBuilder interprets them against current
// state so every persisted event remains a legal production event.
const REPLAY_CASES: u32 = 2_048;
const TAMPER_CASES: u32 = 2_048;
const MAX_RANDOM_ACTIONS: usize = 24;
const MAX_SHRINK_ITERS: u32 = 2_048;
const MAX_FLAT_MAP_REGENS: u32 = 128;
const MAX_DEFAULT_SIZE_RANGE: usize = 24;
const REPLAY_REGRESSIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/proptest-regressions/replay_properties.txt"
);
const TAMPER_REGRESSIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/proptest-regressions/tamper_properties.txt"
);
const REPLAY_SEED: [u8; 32] = *b"rustrace-replay-property-seed-v1";
const TAMPER_SEED: [u8; 32] = *b"rustrace-tamper-property-seed-v1";

const TEXT_TOKENS: &[&str] = &[
    "x",
    "\n",
    "\r\n",
    "é",
    "e\u{301}",
    "猫",
    "🦀",
    "👩‍💻",
    "👍🏽",
    "🇨🇦",
    "مرحبا",
];

#[derive(Clone, Debug)]
struct ActionSeed {
    operation: u8,
    document: u8,
    first_offset: u16,
    second_offset: u16,
    text: u8,
    auxiliary: u8,
}

#[derive(Clone, Debug)]
struct ScenarioSeed {
    initial: u8,
    actions: Vec<ActionSeed>,
}

#[derive(Clone, Debug, Default)]
struct Coverage {
    insertion: bool,
    deletion: bool,
    replacement: bool,
    multiline: bool,
    unicode: bool,
    undo: bool,
    redo: bool,
    creation: bool,
    file_deletion: bool,
    rename_count: usize,
    recreated: bool,
    bof_noop: bool,
    eof_noop: bool,
    undo_exhausted: bool,
    redo_exhausted: bool,
}

impl Coverage {
    fn observe(&mut self, event: &Event) {
        match event {
            Event::FileEdited(transaction) => {
                self.undo |= transaction.origin == EditOrigin::Undo;
                self.redo |= transaction.origin == EditOrigin::Redo;
                for edit in &transaction.edits {
                    self.insertion |=
                        edit.start_byte == edit.end_byte && !edit.inserted_text.is_empty();
                    self.deletion |=
                        edit.start_byte < edit.end_byte && edit.inserted_text.is_empty();
                    self.replacement |=
                        edit.start_byte < edit.end_byte && !edit.inserted_text.is_empty();
                    self.multiline |= edit.inserted_text.contains('\n');
                    self.unicode |= !edit.inserted_text.is_ascii();
                }
            }
            Event::FileCreated(_) => self.creation = true,
            Event::FileDeleted(_) => self.file_deletion = true,
            Event::FileRenamed(_) => self.rename_count += 1,
            _ => {}
        }
    }

    fn verify_required(&self) -> Result<(), String> {
        if self.insertion
            && self.deletion
            && self.replacement
            && self.multiline
            && self.unicode
            && self.undo
            && self.redo
            && self.creation
            && self.file_deletion
            && self.rename_count >= 2
            && self.recreated
            && self.bof_noop
            && self.eof_noop
            && self.undo_exhausted
            && self.redo_exhausted
        {
            Ok(())
        } else {
            Err(format!(
                "generated stream missed required coverage: {self:?}"
            ))
        }
    }
}

#[derive(Clone, Default)]
struct TransactionRecorder(Rc<RefCell<Vec<EditorTransaction>>>);

impl TransactionRecorder {
    fn take_committed(&self) -> Result<EditorTransaction, String> {
        let mut transactions = self.0.borrow_mut();
        if transactions.len() != 1 {
            return Err(format!(
                "editor emitted {} transactions for one mutation",
                transactions.len()
            ));
        }
        Ok(transactions.remove(0))
    }

    fn verify_empty(&self) -> Result<(), String> {
        let count = self.0.borrow().len();
        if count == 0 {
            Ok(())
        } else {
            Err(format!("no-op editor action emitted {count} transactions"))
        }
    }
}

impl EditorEffects for TransactionRecorder {
    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), rustrace_editor::EditorEffectError> {
        self.0.borrow_mut().push(transaction.clone());
        Ok(())
    }
}

struct LiveDocument {
    path: WorkspacePath,
    text: String,
    version: u64,
    selection: SelectionState,
    editor: EditorBuffer<TransactionRecorder>,
    recorder: TransactionRecorder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedDocument {
    path: WorkspacePath,
    text: String,
    version: u64,
    selection: SelectionState,
    content_hash: Hash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedWorkspace {
    files: BTreeMap<WorkspacePath, Vec<u8>>,
    documents: BTreeMap<DocumentId, ExpectedDocument>,
    active_document: Option<DocumentId>,
    workspace_hash: Hash,
}

#[derive(Clone, Debug)]
struct GeneratedCase {
    initial_checkpoint: StoredCheckpoint,
    events: Vec<EventEnvelope>,
    later_checkpoint: StoredCheckpoint,
    checkpoint_event_index: usize,
    expected: ExpectedWorkspace,
}

struct CaseBuilder {
    session_id: SessionId,
    initial_checkpoint: StoredCheckpoint,
    files: BTreeMap<WorkspacePath, Vec<u8>>,
    documents: BTreeMap<DocumentId, LiveDocument>,
    active_document: Option<DocumentId>,
    events: Vec<EventEnvelope>,
    next_sequence: u64,
    previous_hash: Hash,
    path_nonce: u64,
    coverage: Coverage,
    later_checkpoint: Option<StoredCheckpoint>,
    checkpoint_event_index: usize,
}

impl CaseBuilder {
    fn new(initial_selector: u8) -> Result<Self, String> {
        let session_id = SessionId::new(format!("property-{initial_selector}"))
            .map_err(|error| error.to_string())?;
        let definitions: Vec<(&str, &str, &str)> = match initial_selector % 3 {
            0 => vec![("doc-0", "src/main.rs", "")],
            1 => vec![
                ("doc-0", "src/main.rs", "fn main() {}\n"),
                ("doc-1", "notes.txt", "café\n"),
            ],
            _ => vec![
                ("doc-0", "src/main.rs", "e\u{301}\n🦀"),
                ("doc-1", "src/lib.rs", "pub fn 猫() {}\n"),
                ("doc-2", "README.md", "👩‍💻\r\n"),
            ],
        };

        let mut files = BTreeMap::new();
        let mut documents = BTreeMap::new();
        for (id, path, text) in definitions {
            let document_id = document_id(id)?;
            let path = workspace_path(path)?;
            files.insert(path.clone(), text.as_bytes().to_vec());
            let recorder = TransactionRecorder::default();
            let editor = EditorBuffer::new(document_id.clone(), text, recorder.clone());
            documents.insert(
                document_id,
                LiveDocument {
                    path,
                    text: text.to_owned(),
                    version: 0,
                    selection: SelectionState::default(),
                    editor,
                    recorder,
                },
            );
        }
        if initial_selector % 3 == 2 {
            files.insert(workspace_path("assets/raw.bin")?, vec![0xff, 0x00, 0x80]);
        }

        let active_document = documents.keys().next().cloned();
        let snapshot =
            snapshot_from_state(&session_id, 1, &files, &documents, active_document.clone())?;
        let owning_event = seal_event(
            &session_id,
            1,
            Hash::zero(),
            Event::WorkspaceCheckpoint(snapshot.event_payload()),
        )?;
        let previous_hash = owning_event.event_hash;
        let initial_checkpoint = StoredCheckpoint {
            owning_event,
            snapshot,
        };

        Ok(Self {
            session_id,
            initial_checkpoint,
            files,
            documents,
            active_document,
            events: Vec::new(),
            next_sequence: 2,
            previous_hash,
            path_nonce: 0,
            coverage: Coverage::default(),
            later_checkpoint: None,
            checkpoint_event_index: 0,
        })
    }

    fn add_event(&mut self, event: Event) -> Result<EventEnvelope, String> {
        self.coverage.observe(&event);
        let envelope = seal_event(
            &self.session_id,
            self.next_sequence,
            self.previous_hash,
            event,
        )?;
        self.next_sequence += 1;
        self.previous_hash = envelope.event_hash;
        self.events.push(envelope.clone());
        Ok(envelope)
    }

    fn add_checkpoint(&mut self) -> Result<StoredCheckpoint, String> {
        let snapshot = snapshot_from_state(
            &self.session_id,
            self.next_sequence,
            &self.files,
            &self.documents,
            self.active_document.clone(),
        )?;
        let owning_event = self.add_event(Event::WorkspaceCheckpoint(snapshot.event_payload()))?;
        Ok(StoredCheckpoint {
            owning_event,
            snapshot,
        })
    }

    fn select(
        &mut self,
        document_id: &DocumentId,
        selection: SelectionState,
    ) -> Result<(), String> {
        let changed = {
            let document = self
                .documents
                .get_mut(document_id)
                .ok_or_else(|| format!("missing live document {document_id}"))?;
            let changed = document
                .editor
                .set_selection(selection)
                .map_err(|error| error.to_string())?;
            if changed {
                document.selection = selection;
            }
            changed
        };
        if changed {
            self.add_event(Event::SelectionChanged(SelectionChanged {
                document_id: document_id.clone(),
                anchor_byte: selection.anchor_byte,
                active_byte: selection.active_byte,
            }))?;
        }
        Ok(())
    }

    fn edit_document<F>(&mut self, document_id: &DocumentId, edit: F) -> Result<bool, String>
    where
        F: FnOnce(&mut EditorBuffer<TransactionRecorder>) -> Result<bool, String>,
    {
        let committed = {
            let document = self
                .documents
                .get_mut(document_id)
                .ok_or_else(|| format!("missing live document {document_id}"))?;
            let before = document.text.clone();
            let committed = edit(&mut document.editor)?;
            if !committed {
                document.recorder.verify_empty()?;
                return Ok(false);
            }

            let transaction = document.recorder.take_committed()?;
            if transaction.document_id != *document_id
                || transaction.version_before != document.version
                || transaction.version_after != document.version + 1
                || transaction.selection_before != document.selection
                || transaction.hash_before != document_hash(&before)
            {
                return Err(format!(
                    "live transaction metadata diverged from reference state: {transaction:?}"
                ));
            }
            let after = apply_text_edits(&before, &transaction.edits)?;
            if transaction.hash_after != document_hash(&after)
                || document.editor.text() != after
                || document.editor.version() != transaction.version_after
                || document.editor.selection_state() != transaction.selection_after
                || document.editor.hash() != transaction.hash_after
            {
                return Err("live editor result diverged from independent text model".to_owned());
            }

            document.text = after;
            document.version = transaction.version_after;
            document.selection = transaction.selection_after;
            (
                document.path.clone(),
                document.text.as_bytes().to_vec(),
                transaction,
            )
        };

        self.files.insert(committed.0, committed.1);
        self.add_event(Event::FileEdited(committed.2))?;
        Ok(true)
    }

    fn create_document(
        &mut self,
        document_id: DocumentId,
        path: WorkspacePath,
        contents: &str,
    ) -> Result<(), String> {
        if self.documents.contains_key(&document_id) || self.files.contains_key(&path) {
            return Err("reference model attempted an invalid create".to_owned());
        }
        let event = Event::FileCreated(FileCreated {
            document_id: document_id.clone(),
            path: path.clone(),
            contents: contents.to_owned(),
            content_hash: document_hash(contents),
        });
        let recorder = TransactionRecorder::default();
        let editor = EditorBuffer::new(document_id.clone(), contents, recorder.clone());
        self.files
            .insert(path.clone(), contents.as_bytes().to_vec());
        self.documents.insert(
            document_id,
            LiveDocument {
                path,
                text: contents.to_owned(),
                version: 0,
                selection: SelectionState::default(),
                editor,
                recorder,
            },
        );
        self.add_event(event)?;
        Ok(())
    }

    fn delete_document(&mut self, document_id: &DocumentId) -> Result<(), String> {
        let document = self
            .documents
            .remove(document_id)
            .ok_or_else(|| format!("missing live document {document_id}"))?;
        let event = Event::FileDeleted(FileDeleted {
            document_id: document_id.clone(),
            path: document.path.clone(),
            previous_hash: document_hash(&document.text),
        });
        self.files.remove(&document.path);
        if self.active_document.as_ref() == Some(document_id) {
            self.active_document = None;
        }
        self.add_event(event)?;
        Ok(())
    }

    fn rename_document(
        &mut self,
        document_id: &DocumentId,
        new_path: WorkspacePath,
    ) -> Result<(), String> {
        if self.files.contains_key(&new_path) {
            return Err(format!("rename destination {new_path} already exists"));
        }
        let old_path = self
            .documents
            .get(document_id)
            .ok_or_else(|| format!("missing live document {document_id}"))?
            .path
            .clone();
        let contents = self
            .files
            .remove(&old_path)
            .ok_or_else(|| format!("missing reference file {old_path}"))?;
        self.files.insert(new_path.clone(), contents);
        self.documents
            .get_mut(document_id)
            .ok_or_else(|| format!("missing live document {document_id}"))?
            .path = new_path.clone();
        self.add_event(Event::FileRenamed(FileRenamed {
            document_id: document_id.clone(),
            old_path,
            new_path,
        }))?;
        Ok(())
    }

    fn focus_document(&mut self, document_id: &DocumentId) -> Result<(), String> {
        if self.active_document.as_ref() == Some(document_id) {
            return Ok(());
        }
        if !self.documents.contains_key(document_id) {
            return Err(format!("cannot focus missing document {document_id}"));
        }
        self.active_document = Some(document_id.clone());
        self.add_event(Event::FileFocused(FileFocused {
            document_id: document_id.clone(),
        }))?;
        Ok(())
    }

    fn unique_path(&mut self, label: &str) -> Result<WorkspacePath, String> {
        loop {
            let candidate = workspace_path(&format!("generated/{label}-{}.txt", self.path_nonce))?;
            self.path_nonce += 1;
            if !self.files.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
    }

    fn document_for(&self, selector: u8) -> Option<DocumentId> {
        let index = usize::from(selector) % self.documents.len().max(1);
        self.documents.keys().nth(index).cloned()
    }

    fn ensure_document(&mut self, selector: u8, text: u8) -> Result<DocumentId, String> {
        if let Some(document_id) = self.document_for(selector) {
            return Ok(document_id);
        }
        let document_id = document_id("doc-recovery")?;
        let path = self.unique_path("recovery")?;
        self.create_document(document_id.clone(), path, token(text))?;
        Ok(document_id)
    }

    fn apply_required_spine(&mut self) -> Result<(), String> {
        let primary = self
            .documents
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| "initial checkpoint has no primary document".to_owned())?;

        self.select(&primary, SelectionState::caret(0))?;
        self.coverage.bof_noop = !self.edit_document(&primary, |editor| {
            editor.delete_backward().map_err(|error| error.to_string())
        })?;
        let initial_end = self.documents[&primary].text.len() as u64;
        self.select(&primary, SelectionState::caret(initial_end))?;
        self.coverage.eof_noop = !self.edit_document(&primary, |editor| {
            editor.delete_forward().map_err(|error| error.to_string())
        })?;
        self.coverage.undo_exhausted = !self.edit_document(&primary, |editor| {
            editor.undo().map_err(|error| error.to_string())
        })?;
        self.coverage.redo_exhausted = !self.edit_document(&primary, |editor| {
            editor.redo().map_err(|error| error.to_string())
        })?;

        self.edit_document(&primary, |editor| {
            editor.insert_char('🦀').map_err(|error| error.to_string())
        })?;
        let end = self.documents[&primary].text.len() as u64;
        self.select(&primary, SelectionState::caret(end))?;
        self.edit_document(&primary, |editor| {
            editor
                .paste("\nline\ne\u{301}")
                .map_err(|error| error.to_string())
        })?;

        let first_character_end = self.documents[&primary]
            .text
            .chars()
            .next()
            .map_or(0, char::len_utf8) as u64;
        self.select(&primary, SelectionState::new(0, first_character_end))?;
        self.edit_document(&primary, |editor| {
            editor.paste("界").map_err(|error| error.to_string())
        })?;

        let end = self.documents[&primary].text.len() as u64;
        self.select(&primary, SelectionState::caret(end))?;
        self.edit_document(&primary, |editor| {
            editor.delete_backward().map_err(|error| error.to_string())
        })?;
        self.edit_document(&primary, |editor| {
            editor.undo().map_err(|error| error.to_string())
        })?;
        self.edit_document(&primary, |editor| {
            editor.redo().map_err(|error| error.to_string())
        })?;
        self.select(&primary, SelectionState::caret(0))?;
        self.edit_document(&primary, |editor| {
            editor.delete_forward().map_err(|error| error.to_string())
        })?;

        let lifecycle_id = document_id("doc-lifecycle")?;
        let first_path = workspace_path("generated/lifecycle-a.txt")?;
        self.create_document(lifecycle_id.clone(), first_path, "new\n👩‍💻")?;
        self.rename_document(&lifecycle_id, workspace_path("generated/lifecycle-b.txt")?)?;
        let final_path = workspace_path("generated/lifecycle-c.txt")?;
        self.rename_document(&lifecycle_id, final_path.clone())?;
        self.focus_document(&lifecycle_id)?;
        self.delete_document(&lifecycle_id)?;
        self.create_document(lifecycle_id.clone(), final_path, "again\r\n👍🏽")?;
        self.coverage.recreated = true;
        self.focus_document(&lifecycle_id)?;
        Ok(())
    }

    fn apply_action(&mut self, action: &ActionSeed) -> Result<(), String> {
        match action.operation % 12 {
            0 => {
                let id = self.ensure_document(action.document, action.text)?;
                let offset = self.offset(&id, action.first_offset)?;
                self.select(&id, SelectionState::caret(offset))?;
                let character = token(action.text).chars().next().unwrap_or('x');
                self.edit_document(&id, |editor| {
                    editor
                        .insert_char(character)
                        .map_err(|error| error.to_string())
                })?;
            }
            1 => {
                let id = self.ensure_document(action.document, action.text)?;
                let offset = self.offset(&id, action.first_offset)?;
                self.select(&id, SelectionState::caret(offset))?;
                self.edit_document(&id, |editor| {
                    editor
                        .paste(token(action.text))
                        .map_err(|error| error.to_string())
                })?;
            }
            2 | 3 => {
                let id = self.ensure_document(action.document, action.text)?;
                let offset = self.offset(&id, action.first_offset)?;
                self.select(&id, SelectionState::caret(offset))?;
                if action.operation % 12 == 2 {
                    self.edit_document(&id, |editor| {
                        editor.delete_backward().map_err(|error| error.to_string())
                    })?;
                } else {
                    self.edit_document(&id, |editor| {
                        editor.delete_forward().map_err(|error| error.to_string())
                    })?;
                }
            }
            4 => {
                let id = self.ensure_document(action.document, action.text)?;
                let boundaries = char_boundaries(&self.documents[&id].text);
                if boundaries.len() <= 1 {
                    self.edit_document(&id, |editor| {
                        editor
                            .paste(token(action.text))
                            .map_err(|error| error.to_string())
                    })?;
                } else {
                    let first = usize::from(action.first_offset) % (boundaries.len() - 1);
                    let span = usize::from(action.second_offset) % (boundaries.len() - first - 1);
                    let mut selection = SelectionState::new(
                        boundaries[first] as u64,
                        boundaries[first + span + 1] as u64,
                    );
                    if action.auxiliary % 2 == 1 {
                        selection =
                            SelectionState::new(selection.active_byte, selection.anchor_byte);
                    }
                    self.select(&id, selection)?;
                    self.edit_document(&id, |editor| {
                        editor
                            .paste(token(action.text))
                            .map_err(|error| error.to_string())
                    })?;
                }
            }
            5 | 6 => {
                if let Some(id) = self.document_for(action.document) {
                    if action.operation % 12 == 5 {
                        self.edit_document(&id, |editor| {
                            editor.undo().map_err(|error| error.to_string())
                        })?;
                    } else {
                        self.edit_document(&id, |editor| {
                            editor.redo().map_err(|error| error.to_string())
                        })?;
                    }
                }
            }
            7 => {
                if let Some(id) = (0..8)
                    .map(|slot| document_id(&format!("doc-generated-{slot}")))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .find(|id| !self.documents.contains_key(id))
                {
                    let path = self.unique_path("created")?;
                    self.create_document(id, path, token(action.text))?;
                }
            }
            8 => {
                if let Some(id) = self.document_for(action.document) {
                    self.delete_document(&id)?;
                }
            }
            9 => {
                if let Some(id) = self.document_for(action.document) {
                    let path = self.unique_path("renamed")?;
                    self.rename_document(&id, path)?;
                }
            }
            10 => {
                if let Some(id) = self.document_for(action.document) {
                    self.focus_document(&id)?;
                }
            }
            11 => {
                if let Some(id) = self.document_for(action.document) {
                    let anchor = self.offset(&id, action.first_offset)?;
                    let active = self.offset(&id, action.second_offset)?;
                    self.select(&id, SelectionState::new(anchor, active))?;
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn offset(&self, document_id: &DocumentId, selector: u16) -> Result<u64, String> {
        let text = &self
            .documents
            .get(document_id)
            .ok_or_else(|| format!("missing live document {document_id}"))?
            .text;
        let boundaries = char_boundaries(text);
        Ok(boundaries[usize::from(selector) % boundaries.len()] as u64)
    }

    fn finalize(mut self) -> Result<GeneratedCase, String> {
        self.coverage.verify_required()?;
        let final_hash = workspace_hash(&self.files)?;
        let sequence = self.next_sequence;
        self.add_event(Event::SubmissionFinalized(SubmissionFinalized {
            final_workspace_hash: final_hash,
            event_count: sequence,
            clean: true,
            warnings: Vec::new(),
        }))?;

        let later_checkpoint = self
            .later_checkpoint
            .take()
            .ok_or_else(|| "generated case is missing its later checkpoint".to_owned())?;
        let checkpoint_event_index = self.checkpoint_event_index;
        let expected = ExpectedWorkspace {
            files: self.files,
            documents: self
                .documents
                .into_iter()
                .map(|(id, document)| {
                    let expected = ExpectedDocument {
                        path: document.path,
                        content_hash: document_hash(&document.text),
                        text: document.text,
                        version: document.version,
                        selection: document.selection,
                    };
                    (id, expected)
                })
                .collect(),
            active_document: self.active_document,
            workspace_hash: final_hash,
        };
        Ok(GeneratedCase {
            initial_checkpoint: self.initial_checkpoint,
            events: self.events,
            later_checkpoint,
            checkpoint_event_index,
            expected,
        })
    }
}

fn action_strategy() -> impl Strategy<Value = ActionSeed> {
    (
        any::<u8>(),
        any::<u8>(),
        any::<u16>(),
        any::<u16>(),
        any::<u8>(),
        any::<u8>(),
    )
        .prop_map(
            |(operation, document, first_offset, second_offset, text, auxiliary)| ActionSeed {
                operation,
                document,
                first_offset,
                second_offset,
                text,
                auxiliary,
            },
        )
}

fn scenario_strategy() -> impl Strategy<Value = ScenarioSeed> {
    (0_u8..3, vec(action_strategy(), 0..=MAX_RANDOM_ACTIONS))
        .prop_map(|(initial, actions)| ScenarioSeed { initial, actions })
}

fn build_case(seed: &ScenarioSeed) -> Result<GeneratedCase, String> {
    let mut builder = CaseBuilder::new(seed.initial)?;
    builder.apply_required_spine()?;
    let midpoint = seed.actions.len() / 2;
    for action in &seed.actions[..midpoint] {
        builder.apply_action(action)?;
    }
    builder.checkpoint_event_index = builder.events.len();
    builder.later_checkpoint = Some(builder.add_checkpoint()?);
    for action in &seed.actions[midpoint..] {
        builder.apply_action(action)?;
    }
    builder.finalize()
}

fn snapshot_from_state(
    session_id: &SessionId,
    sequence: u64,
    files: &BTreeMap<WorkspacePath, Vec<u8>>,
    documents: &BTreeMap<DocumentId, LiveDocument>,
    active_document: Option<DocumentId>,
) -> Result<CheckpointSnapshot, String> {
    let checkpoint_files = files
        .iter()
        .map(|(path, contents)| CheckpointFile {
            path: path.clone(),
            contents: contents.clone(),
        })
        .collect();
    let open_documents = documents
        .iter()
        .map(|(document_id, document)| OpenDocument {
            document_id: document_id.clone(),
            path: document.path.clone(),
            selection: document.selection,
            version: document.version,
        })
        .collect();
    CheckpointSnapshot::new(
        session_id.clone(),
        sequence,
        checkpoint_files,
        active_document,
        open_documents,
    )
    .map_err(|error| error.to_string())
}

fn seal_event(
    session_id: &SessionId,
    sequence: u64,
    previous_event_hash: Hash,
    event: Event,
) -> Result<EventEnvelope, String> {
    EventEnvelope {
        format_version: FORMAT_VERSION_V1,
        session_id: session_id.clone(),
        sequence,
        monotonic_millis: sequence,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(previous_event_hash)
    .map_err(|error| error.to_string())
}

fn apply_text_edits(before: &str, edits: &[TextEdit]) -> Result<String, String> {
    let mut after = before.to_owned();
    for edit in edits.iter().rev() {
        let start = usize::try_from(edit.start_byte)
            .map_err(|_| "edit start does not fit usize".to_owned())?;
        let end =
            usize::try_from(edit.end_byte).map_err(|_| "edit end does not fit usize".to_owned())?;
        if start > end
            || end > before.len()
            || !before.is_char_boundary(start)
            || !before.is_char_boundary(end)
        {
            return Err(format!(
                "editor emitted invalid reference range {start}..{end} for {} bytes",
                before.len()
            ));
        }
        after.replace_range(start..end, &edit.inserted_text);
    }
    Ok(after)
}

fn char_boundaries(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect()
}

fn token(selector: u8) -> &'static str {
    TEXT_TOKENS[usize::from(selector) % TEXT_TOKENS.len()]
}

fn document_id(value: &str) -> Result<DocumentId, String> {
    DocumentId::new(value).map_err(|error| error.to_string())
}

fn workspace_path(value: &str) -> Result<WorkspacePath, String> {
    WorkspacePath::new(value).map_err(|error| error.to_string())
}

fn workspace_hash(files: &BTreeMap<WorkspacePath, Vec<u8>>) -> Result<Hash, String> {
    hash_entries(
        files
            .iter()
            .map(|(path, contents)| (path, contents.as_slice())),
    )
    .map_err(|error| error.to_string())
}

fn property_runner(cases: u32, seed: [u8; 32], persistence: &'static str) -> TestRunner {
    let config = Config {
        cases,
        max_local_rejects: 128,
        max_global_rejects: 128,
        max_flat_map_regens: MAX_FLAT_MAP_REGENS,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(persistence))),
        source_file: Some(file!()),
        max_shrink_time: 0,
        max_shrink_iters: MAX_SHRINK_ITERS,
        max_default_size_range: MAX_DEFAULT_SIZE_RANGE,
        ..Config::default()
    };
    TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &seed))
}

fn verify_generated_case(case: &GeneratedCase) -> Result<(), String> {
    let mut full = ReplayEngine::from_initial_checkpoint(case.initial_checkpoint.clone())
        .map_err(|error| error.to_string())?;
    let mut certificate = None;
    for (index, event) in case.events.iter().enumerate() {
        full.apply(event).map_err(|error| {
            format!(
                "continuous replay rejected sequence {}: {error}",
                event.sequence
            )
        })?;
        if index == case.checkpoint_event_index {
            certificate = Some(
                full.certify_checkpoint(case.later_checkpoint.clone())
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    assert_expected_state(&full, &case.expected)?;
    let recorded_tip = case
        .events
        .last()
        .ok_or_else(|| "generated journal contains no suffix events".to_owned())?;
    if full.next_sequence() != recorded_tip.sequence + 1
        || full.last_event_hash() != recorded_tip.event_hash
    {
        return Err(format!(
            "continuous replay cursor differs from recorded tip at sequence {}",
            recorded_tip.sequence
        ));
    }

    let certificate = certificate.ok_or_else(|| "checkpoint was not certified".to_owned())?;
    let mut seek = ReplayEngine::from_checkpoint(certificate);
    for event in &case.events[case.checkpoint_event_index + 1..] {
        seek.apply(event).map_err(|error| {
            format!(
                "suffix replay rejected sequence {}: {error}",
                event.sequence
            )
        })?;
    }
    assert_expected_state(&seek, &case.expected)?;
    if seek.workspace_state() != full.workspace_state()
        || seek.next_sequence() != full.next_sequence()
        || seek.last_event_hash() != full.last_event_hash()
        || seek.is_terminal() != full.is_terminal()
        || seek.is_finalized() != full.is_finalized()
    {
        return Err("certified checkpoint suffix diverged from continuous replay".to_owned());
    }
    Ok(())
}

fn assert_expected_state(
    replay: &ReplayEngine,
    expected: &ExpectedWorkspace,
) -> Result<(), String> {
    let state = replay.workspace_state();
    if state.files() != &expected.files {
        return Err(format!(
            "replayed file bytes differ: actual={:?}, expected={:?}",
            state.files(),
            expected.files
        ));
    }
    if state.documents().len() != expected.documents.len() {
        return Err(format!(
            "replayed document count is {}; expected {}",
            state.documents().len(),
            expected.documents.len()
        ));
    }
    for (document_id, expected_document) in &expected.documents {
        let actual = state
            .document(document_id)
            .ok_or_else(|| format!("replay omitted document {document_id}"))?;
        if actual.path() != &expected_document.path
            || actual.text() != expected_document.text
            || actual.version() != expected_document.version
            || actual.selection() != expected_document.selection
            || actual.content_hash() != expected_document.content_hash
        {
            return Err(format!(
                "replayed metadata differs for {document_id}: path={}, text={:?}, version={}, selection={:?}, hash={}",
                actual.path(),
                actual.text(),
                actual.version(),
                actual.selection(),
                actual.content_hash()
            ));
        }
    }
    if state.active_document() != expected.active_document.as_ref()
        || state.workspace_hash() != expected.workspace_hash
        || replay.current_workspace_hash() != expected.workspace_hash
        || !replay.is_terminal()
        || !replay.is_finalized()
    {
        return Err(format!(
            "replayed workspace metadata differs: active={:?}, workspace_hash={}, terminal={}, finalized={}",
            state.active_document(),
            state.workspace_hash(),
            replay.is_terminal(),
            replay.is_finalized()
        ));
    }
    replay
        .verify_final_workspace_hash(expected.workspace_hash)
        .map_err(|error| error.to_string())
}

fn flip_hash(hash: Hash) -> Hash {
    let mut bytes = *hash.as_bytes();
    bytes[0] ^= 1;
    Hash::from_bytes(bytes)
}

fn rehashed_semantic_tamper(envelope: &mut EventEnvelope) -> Result<(), String> {
    match &mut envelope.event {
        Event::FileEdited(transaction) => {
            transaction.version_before += 1;
            transaction.version_after += 1;
        }
        Event::FileCreated(event) => event.content_hash = flip_hash(event.content_hash),
        Event::FileDeleted(event) => event.previous_hash = flip_hash(event.previous_hash),
        Event::FileRenamed(event) => {
            event.old_path = workspace_path("tampered/missing-source.txt")?;
        }
        Event::FileFocused(event) => event.document_id = document_id("tampered-missing")?,
        Event::SelectionChanged(event) => event.active_byte = u64::MAX,
        Event::WorkspaceCheckpoint(event) => {
            event.workspace_hash = flip_hash(event.workspace_hash);
        }
        Event::SubmissionFinalized(event) => {
            event.final_workspace_hash = flip_hash(event.final_workspace_hash);
        }
        unexpected => {
            return Err(format!(
                "property generator emitted unsupported event for tampering: {unexpected:?}"
            ));
        }
    }
    let previous_hash = envelope.previous_event_hash;
    *envelope = envelope
        .clone()
        .seal(previous_hash)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn rehashed_edit_range_tamper(envelope: &mut EventEnvelope) -> Result<(), String> {
    let Event::FileEdited(transaction) = &mut envelope.event else {
        return Err("range tamper did not select a file edit".to_owned());
    };
    let edit = transaction
        .edits
        .first_mut()
        .ok_or_else(|| "committed transaction has no edits".to_owned())?;
    edit.start_byte = u64::MAX;
    edit.end_byte = u64::MAX;
    let previous_hash = envelope.previous_event_hash;
    *envelope = envelope
        .clone()
        .seal(previous_hash)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn rehashed_transaction_hash_tamper(envelope: &mut EventEnvelope) -> Result<(), String> {
    let Event::FileEdited(transaction) = &mut envelope.event else {
        return Err("hash tamper did not select a file edit".to_owned());
    };
    transaction.hash_before = flip_hash(transaction.hash_before);
    let previous_hash = envelope.previous_event_hash;
    *envelope = envelope
        .clone()
        .seal(previous_hash)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_tamper_case(
    case: &GeneratedCase,
    event_selector: u32,
    tamper_selector: u8,
) -> Result<(), String> {
    let mode = tamper_selector % 4;
    let file_edit_indices: Vec<_> = case
        .events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| matches!(event.event, Event::FileEdited(_)).then_some(index))
        .collect();
    let target = if mode >= 2 {
        file_edit_indices[usize::try_from(event_selector).unwrap_or(0) % file_edit_indices.len()]
    } else {
        usize::try_from(event_selector).unwrap_or(0) % case.events.len()
    };

    let mut replay = ReplayEngine::from_initial_checkpoint(case.initial_checkpoint.clone())
        .map_err(|error| error.to_string())?;
    for event in &case.events[..target] {
        replay.apply(event).map_err(|error| error.to_string())?;
    }
    let state_before = replay.workspace_state().clone();
    let sequence_before = replay.next_sequence();
    let hash_before = replay.last_event_hash();
    let terminal_before = replay.is_terminal();
    let finalized_before = replay.is_finalized();

    let mut tampered = case.events[target].clone();
    match mode {
        0 => tampered.event_hash = flip_hash(tampered.event_hash),
        1 => rehashed_semantic_tamper(&mut tampered)?,
        2 => rehashed_edit_range_tamper(&mut tampered)?,
        3 => rehashed_transaction_hash_tamper(&mut tampered)?,
        _ => unreachable!(),
    }
    let error = replay
        .apply(&tampered)
        .expect_err("tampered event unexpectedly passed validation");
    if mode != 0
        && matches!(
            error,
            ReplayError::EventHashMismatch { .. } | ReplayError::PreviousEventHashMismatch { .. }
        )
    {
        return Err(format!(
            "rehashed semantic tamper was rejected only by envelope validation: {error}"
        ));
    }
    if replay.workspace_state() != &state_before
        || replay.next_sequence() != sequence_before
        || replay.last_event_hash() != hash_before
        || replay.is_terminal() != terminal_before
        || replay.is_finalized() != finalized_before
    {
        return Err(format!(
            "failed tamper advanced replay state at sequence {sequence_before}"
        ));
    }
    replay
        .apply(&case.events[target])
        .map_err(|error| format!("original event failed after rejected tamper: {error}"))?;
    Ok(())
}

fn run_generated_replay_cases() -> TestCaseResult {
    let mut runner = property_runner(REPLAY_CASES, REPLAY_SEED, REPLAY_REGRESSIONS);
    runner
        .run(&scenario_strategy(), |seed| {
            let case = build_case(&seed).map_err(TestCaseError::fail)?;
            verify_generated_case(&case).map_err(TestCaseError::fail)
        })
        .map_err(|error| TestCaseError::fail(format!("{error:?}")))
}

fn run_generated_tamper_cases() -> TestCaseResult {
    let strategy = (scenario_strategy(), any::<u32>(), any::<u8>());
    let mut runner = property_runner(TAMPER_CASES, TAMPER_SEED, TAMPER_REGRESSIONS);
    runner
        .run(&strategy, |(seed, event_selector, tamper_selector)| {
            let case = build_case(&seed).map_err(TestCaseError::fail)?;
            verify_tamper_case(&case, event_selector, tamper_selector).map_err(TestCaseError::fail)
        })
        .map_err(|error| TestCaseError::fail(format!("{error:?}")))
}

#[test]
fn focused_boundary_scenarios_replay() {
    let seeds = [
        ScenarioSeed {
            initial: 0,
            actions: vec![
                ActionSeed {
                    operation: 2,
                    document: 0,
                    first_offset: 0,
                    second_offset: 0,
                    text: 1,
                    auxiliary: 0,
                },
                ActionSeed {
                    operation: 3,
                    document: 0,
                    first_offset: u16::MAX,
                    second_offset: u16::MAX,
                    text: 2,
                    auxiliary: 0,
                },
                ActionSeed {
                    operation: 4,
                    document: 0,
                    first_offset: 0,
                    second_offset: u16::MAX,
                    text: 7,
                    auxiliary: 1,
                },
            ],
        },
        ScenarioSeed {
            initial: 2,
            actions: (0..16)
                .map(|index| ActionSeed {
                    operation: if index < 8 { 5 } else { 6 },
                    document: 0,
                    first_offset: index,
                    second_offset: u16::MAX - index,
                    text: index as u8,
                    auxiliary: index as u8,
                })
                .collect(),
        },
    ];

    for seed in seeds {
        let case = build_case(&seed).expect("focused scenario must be valid");
        verify_generated_case(&case).expect("focused scenario must replay exactly");
        for selector in 0..4 {
            verify_tamper_case(&case, selector, selector as u8)
                .expect("focused tamper must be rejected atomically");
        }
    }
}

#[test]
fn generated_journals_replay_exactly_from_initial_and_later_checkpoints() -> TestCaseResult {
    run_generated_replay_cases()
}

#[test]
fn generated_event_tampering_is_rejected_atomically() -> TestCaseResult {
    run_generated_tamper_cases()
}
