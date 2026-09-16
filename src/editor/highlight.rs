use std::error::Error;
use std::fmt;
use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};

use tree_sitter::{
    ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator,
};

// Local derived-work limits, independent of journal/session budgets.
const EDIT_SYNTAX_BUDGET: Duration = Duration::from_millis(20);
const OPEN_SYNTAX_BUDGET: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HighlightKind {
    Keyword,
    Function,
    String,
    Comment,
    Type,
    Constant,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HighlightSpan {
    pub byte_range: Range<usize>,
    pub kind: HighlightKind,
}

#[derive(Debug)]
pub struct HighlightError {
    message: String,
}

impl HighlightError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HighlightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HighlightError {}

/// Compatibility helper for the original one-shot spike callers.
pub struct RustHighlighter(LanguageHighlighter);

impl RustHighlighter {
    pub fn new() -> Result<Self, HighlightError> {
        Ok(Self(LanguageHighlighter::new(LanguageKind::Rust)?))
    }

    pub fn highlight(&mut self, source: &str) -> Result<Vec<HighlightSpan>, HighlightError> {
        let tree = self.0.parse(source, None, None)?;
        self.0.spans(source, tree.root_node(), None)
    }
}

/// One-shot highlighting for a reconstructed read-only file. Unsupported
/// extensions intentionally fall back to plain text.
pub fn highlight_path(
    path: &std::path::Path,
    source: &str,
) -> Result<Vec<HighlightSpan>, HighlightError> {
    let kind = LanguageKind::for_path(path);
    if kind == LanguageKind::Plain {
        return Ok(Vec::new());
    }
    let mut highlighter = LanguageHighlighter::new(kind)?;
    let deadline = Instant::now() + OPEN_SYNTAX_BUDGET;
    let tree = highlighter.parse(source, None, Some(deadline))?;
    highlighter.spans(source, tree.root_node(), Some(deadline))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LanguageKind {
    Rust,
    Toml,
    Plain,
}

impl LanguageKind {
    fn for_path(path: &std::path::Path) -> Self {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") => Self::Rust,
            Some("toml") => Self::Toml,
            _ => Self::Plain,
        }
    }
}

struct LanguageHighlighter {
    parser: Parser,
    query: Query,
}

impl LanguageHighlighter {
    fn new(kind: LanguageKind) -> Result<Self, HighlightError> {
        let (language, query) = match kind {
            LanguageKind::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            ),
            LanguageKind::Toml => (
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            ),
            LanguageKind::Plain => return Err(HighlightError::new("unsupported language")),
        };
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|error| HighlightError::new(error.to_string()))?;
        let query =
            Query::new(&language, query).map_err(|error| HighlightError::new(error.to_string()))?;
        Ok(Self { parser, query })
    }

    fn parse(
        &mut self,
        source: &str,
        old_tree: Option<&tree_sitter::Tree>,
        deadline: Option<Instant>,
    ) -> Result<tree_sitter::Tree, HighlightError> {
        let mut progress = |_: &tree_sitter::ParseState| budget_progress(deadline);
        self.parser
            .parse_with_options(
                &mut |byte, _| &source.as_bytes()[byte..],
                old_tree,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            )
            .ok_or_else(|| {
                // A cancelled parse must not resume against the next transaction.
                self.parser.reset();
                HighlightError::new("Tree-sitter parsing failed or was cancelled")
            })
    }

    fn spans(
        &self,
        source: &str,
        node: tree_sitter::Node<'_>,
        deadline: Option<Instant>,
    ) -> Result<Vec<HighlightSpan>, HighlightError> {
        let capture_names = self.query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut progress = |_: &tree_sitter::QueryCursorState| budget_progress(deadline);
        let mut captures = cursor.captures_with_options(
            &self.query,
            node,
            source.as_bytes(),
            QueryCursorOptions::new().progress_callback(&mut progress),
        );
        let mut spans = Vec::new();
        while let Some((query_match, capture_index)) = captures.next() {
            let capture = query_match.captures()[*capture_index];
            let range = capture.node.byte_range();
            if !range.is_empty() {
                spans.push(HighlightSpan {
                    byte_range: range,
                    kind: classify(capture_names[capture.index as usize]),
                });
            }
        }
        drop(captures);
        if budget_progress(deadline).is_break() || cursor.did_exceed_match_limit() {
            return Err(HighlightError::new("syntax query budget exhausted"));
        }
        spans.sort_by_key(|span| (span.byte_range.start, span.byte_range.end));
        spans.dedup();
        Ok(spans)
    }
}

fn budget_progress(deadline: Option<Instant>) -> ControlFlow<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

use rustrace_editor::{EditorEffectError, EditorEffects};
use rustrace_model::{DocumentId, EditorTransaction};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tree_sitter::{InputEdit, Point, Tree};

/// One derived snapshot per open buffer, never journaled or retained as history.
pub(crate) struct SyntaxState {
    document_id: DocumentId,
    version: u64,
    source: String,
    language: LanguageKind,
    highlighter: Option<LanguageHighlighter>,
    tree: Option<Tree>,
    pub(crate) spans: Vec<HighlightSpan>,
    captures: HashMap<String, CaptureSlice>,
}

// Offsets into the current span vector, not another retained tree or history.
struct CaptureSlice {
    kind_id: u16,
    start_byte: usize,
    spans: Range<usize>,
}

impl SyntaxState {
    pub(crate) fn from_recovered(
        document_id: DocumentId,
        path: &std::path::Path,
        source: &str,
        version: u64,
    ) -> Self {
        let mut state = Self::new(document_id, path, source);
        state.version = version;
        state
    }

    pub(crate) fn new(document_id: DocumentId, path: &std::path::Path, source: &str) -> Self {
        let language = LanguageKind::for_path(path);
        let mut state = Self {
            document_id,
            version: 0,
            source: source.into(),
            language,
            highlighter: LanguageHighlighter::new(language).ok(),
            tree: None,
            spans: Vec::new(),
            captures: HashMap::new(),
        };
        let _ = state.refresh();
        state
    }

    pub(crate) fn rename(&mut self, path: &std::path::Path) {
        let language = LanguageKind::for_path(path);
        if language != self.language {
            self.language = language;
            self.highlighter = LanguageHighlighter::new(language).ok();
            self.tree = None;
            self.captures.clear();
            let _ = self.refresh();
        }
    }

    fn refresh(&mut self) -> Result<(), HighlightError> {
        self.refresh_with_budget(OPEN_SYNTAX_BUDGET)
    }

    fn refresh_with_budget(&mut self, budget: Duration) -> Result<(), HighlightError> {
        let result = self.refresh_until(Instant::now() + budget);
        if result.is_err() {
            self.spans.clear();
            self.captures.clear();
        }
        result
    }

    fn refresh_until(&mut self, deadline: Instant) -> Result<(), HighlightError> {
        let previous_spans = std::mem::take(&mut self.spans);
        let previous_captures = std::mem::take(&mut self.captures);
        let Some(highlighter) = &mut self.highlighter else {
            self.tree = None;
            return Ok(());
        };
        // Take ownership: on failure there can be no stale, edited tree left.
        let old_tree = self.tree.take();
        let tree = highlighter.parse(&self.source, old_tree.as_ref(), Some(deadline))?;
        if budget_progress(Some(deadline)).is_break() {
            return Err(HighlightError::new("syntax parse budget exhausted"));
        }
        if tree.root_node().has_error() {
            // Error nodes do not invalidate captures elsewhere in the tree.
            // Query the complete tree because an error can change root-child
            // boundaries, so the normal child cache is not a safe reuse unit.
            self.spans = highlighter.spans(&self.source, tree.root_node(), Some(deadline))?;
        } else {
            // Both pinned queries are local to root children (Rust items and
            // TOML pairs/tables/comments). They have no root or sibling-context
            // patterns. Exact text and node kind certify the same captures and
            // predicates regardless of absolute position. Unlike Rust, TOML's
            // flat pairs need not retain node identities after incremental parse.
            // Keys cover disjoint root children, bounded by current source bytes;
            // values index one current span vector. Revisit if queries change.
            let mut cursor = tree.walk();
            for node in tree.root_node().children(&mut cursor) {
                if budget_progress(Some(deadline)).is_break() {
                    return Err(HighlightError::new("syntax budget exhausted"));
                }
                let start = self.spans.len();
                let text = &self.source[node.byte_range()];
                if let Some(cached) = previous_captures
                    .get(text)
                    .filter(|cached| cached.kind_id == node.kind_id())
                {
                    self.spans
                        .extend(previous_spans[cached.spans.clone()].iter().map(|span| {
                            HighlightSpan {
                                byte_range: (node.start_byte() + span.byte_range.start
                                    - cached.start_byte)
                                    ..(node.start_byte() + span.byte_range.end - cached.start_byte),
                                kind: span.kind,
                            }
                        }));
                } else {
                    self.spans
                        .extend(highlighter.spans(&self.source, node, Some(deadline))?);
                }
                if !self.captures.contains_key(text) {
                    self.captures.insert(
                        text.to_owned(),
                        CaptureSlice {
                            kind_id: node.kind_id(),
                            start_byte: node.start_byte(),
                            spans: start..self.spans.len(),
                        },
                    );
                }
            }
        }
        self.tree = Some(tree);
        Ok(())
    }

    fn apply(&mut self, transaction: &EditorTransaction) -> Result<(), HighlightError> {
        if transaction.document_id != self.document_id || transaction.version_before != self.version
        {
            self.tree = None;
            self.spans.clear();
            self.captures.clear();
            return Err(HighlightError::new("syntax document/version mismatch"));
        }
        // Committed transactions have already passed the editor's complete
        // validation. Ranges refer to the BEFORE snapshot; reverse order keeps
        // each lower range valid, including multiple inserts at one offset.
        for edit in transaction.edits.iter().rev() {
            let start = edit.start_byte as usize;
            let end = edit.end_byte as usize;
            let start_position = point_at(&self.source, start);
            let old_end_position = point_at(&self.source, end);
            let inserted_end = point_at(&edit.inserted_text, edit.inserted_text.len());
            let new_end_position = Point::new(
                start_position.row + inserted_end.row,
                if inserted_end.row == 0 {
                    start_position.column + inserted_end.column
                } else {
                    inserted_end.column
                },
            );
            if let Some(tree) = &mut self.tree {
                tree.edit(&InputEdit {
                    start_byte: start,
                    old_end_byte: end,
                    new_end_byte: start + edit.inserted_text.len(),
                    start_position,
                    old_end_position,
                    new_end_position,
                });
            }
            self.source.replace_range(start..end, &edit.inserted_text);
        }
        self.version = transaction.version_after;
        self.refresh_with_budget(EDIT_SYNTAX_BUDGET)
    }
}

fn point_at(source: &str, byte: usize) -> Point {
    let prefix = &source.as_bytes()[..byte];
    Point::new(
        prefix.iter().filter(|byte| **byte == b'\n').count(),
        prefix
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(byte, |newline| byte - newline - 1),
    )
}

/// Adds derived syntax updates at the existing post-commit syntax hook.
/// Provenance failures propagate unchanged; highlighting failures only clear
/// rendering spans. The wrapped hooks still receive the same transaction once.
pub struct SyntaxEffects<E> {
    inner: E,
    state: Rc<RefCell<SyntaxState>>,
}

impl<E> SyntaxEffects<E> {
    pub(crate) fn new(inner: E, state: Rc<RefCell<SyntaxState>>) -> Self {
        Self { inner, state }
    }
}

impl<E: EditorEffects> EditorEffects for SyntaxEffects<E> {
    fn check_input_origin(
        &mut self,
        origin: rustrace_model::EditOrigin,
    ) -> Result<(), EditorEffectError> {
        self.inner.check_input_origin(origin)
    }

    fn record_provenance(
        &mut self,
        transaction: &EditorTransaction,
    ) -> Result<(), EditorEffectError> {
        self.inner.record_provenance(transaction)
    }
    fn update_tree_sitter(&mut self, transaction: &EditorTransaction) {
        let _ = self.state.borrow_mut().apply(transaction);
        self.inner.update_tree_sitter(transaction);
    }
    fn send_lsp_did_change(&mut self, transaction: &EditorTransaction) {
        self.inner.send_lsp_did_change(transaction);
    }
    fn record_replay(&mut self, transaction: &EditorTransaction) {
        self.inner.record_replay(transaction);
    }
}

fn classify(capture: &str) -> HighlightKind {
    let root = capture.split('.').next().unwrap_or(capture);
    match root {
        "keyword" => HighlightKind::Keyword,
        "function" | "method" => HighlightKind::Function,
        "string" | "character" => HighlightKind::String,
        "comment" => HighlightKind::Comment,
        "type" | "constructor" => HighlightKind::Type,
        "constant" | "number" | "boolean" => HighlightKind::Constant,
        _ => HighlightKind::Other,
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;
    use crate::editor::{EditorBuffer, NoopEditorEffects};
    use rustrace_model::{DocumentId, EditOrigin, SelectionState, TextEdit};
    use std::path::Path;

    fn insert(
        state: &mut SyntaxState,
        editor: &mut EditorBuffer<NoopEditorEffects>,
        byte: usize,
        text: &str,
    ) {
        let transaction = editor
            .preview_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: byte as u64,
                    end_byte: byte as u64,
                    inserted_text: text.into(),
                }],
                SelectionState::caret((byte + text.len()) as u64),
            )
            .unwrap()
            .unwrap();
        editor.apply_transaction(transaction.clone()).unwrap();
        state.apply(&transaction).unwrap();
    }

    fn assert_span_at(state: &SyntaxState, source: &str, needle: &str, kind: HighlightKind) {
        let byte = source.find(needle).unwrap();
        assert!(
            state.spans.iter().any(|span| {
                span.kind == kind
                    && span.byte_range.start <= byte
                    && span.byte_range.end >= byte + needle.len()
            }),
            "missing {kind:?} highlight for {needle:?} in {:?}",
            state.spans
        );
    }

    #[test]
    fn rust_and_toml_keep_untouched_tokens_when_new_input_is_invalid() {
        for (path, source, needle, kind) in [
            (
                "main.rs",
                "fn main() {\n    let message = \"kept\";\n}",
                "kept",
                HighlightKind::String,
            ),
            (
                "Cargo.toml",
                "[package]\nname = \"kept\"\nversion = 1",
                "kept",
                HighlightKind::String,
            ),
        ] {
            let id = DocumentId::new(path).unwrap();
            let mut state = SyntaxState::new(id.clone(), Path::new(path), source);
            let mut editor = EditorBuffer::new(id, source, NoopEditorEffects);
            assert_span_at(&state, source, needle, kind);

            insert(&mut state, &mut editor, source.len(), "\nif");

            assert!(state.tree.as_ref().unwrap().root_node().has_error());
            assert_span_at(&state, &editor.text(), needle, kind);
        }
    }

    #[test]
    fn invalid_initial_documents_and_crlf_edits_keep_valid_tokens() {
        for (path, source, needle) in [
            (
                "main.rs",
                "if\r\nfn intact() {\r\n    let message = \"kept\";\r\n}\r\n",
                "kept",
            ),
            (
                "Cargo.toml",
                "if\r\n[package]\r\nname = \"kept\"\r\nversion = 1\r\n",
                "kept",
            ),
        ] {
            let id = DocumentId::new(path).unwrap();
            let mut state = SyntaxState::new(id.clone(), Path::new(path), source);
            let mut editor = EditorBuffer::new(id, source, NoopEditorEffects);
            assert!(state.tree.as_ref().unwrap().root_node().has_error());
            assert_span_at(&state, source, needle, HighlightKind::String);

            let end = editor.text().len();
            insert(&mut state, &mut editor, end, "if\r\n");

            assert_span_at(&state, &editor.text(), needle, HighlightKind::String);
        }
    }

    #[test]
    fn error_repair_recovers_complete_highlights_after_an_uncached_parse() {
        let source = "fn intact() { let message = \"kept\"; }";
        let id = DocumentId::new("repair").unwrap();
        let mut state = SyntaxState::new(id.clone(), Path::new("main.rs"), source);
        let mut editor = EditorBuffer::new(id, source, NoopEditorEffects);
        state.captures.clear();
        insert(&mut state, &mut editor, source.len(), "\nif");
        assert_span_at(&state, &editor.text(), "kept", HighlightKind::String);

        let invalid_start = source.len();
        let transaction = editor
            .preview_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: invalid_start as u64,
                    end_byte: editor.text().len() as u64,
                    inserted_text: "\nfn repaired() {}".into(),
                }],
                SelectionState::caret(0),
            )
            .unwrap()
            .unwrap();
        editor.apply_transaction(transaction.clone()).unwrap();
        state.apply(&transaction).unwrap();

        assert!(!state.tree.as_ref().unwrap().root_node().has_error());
        assert_span_at(&state, &editor.text(), "repaired", HighlightKind::Function);
        assert_whole_query(&mut state);
    }

    #[test]
    fn edited_old_tree_reuses_unchanged_subtrees_and_matches_fresh() {
        let source = (0..100)
            .map(|i| format!("fn f{i}() {{ let s = \"世界\"; }}\n"))
            .collect::<String>();
        let id = DocumentId::new("rust").unwrap();
        let mut state = SyntaxState::new(id.clone(), Path::new("a.rs"), &source);
        let unchanged_id = state
            .tree
            .as_ref()
            .unwrap()
            .root_node()
            .named_child(90)
            .unwrap()
            .id();
        let mut editor = EditorBuffer::new(id, &source, NoopEditorEffects);
        let transaction = editor
            .preview_edits(
                EditOrigin::Keyboard,
                vec![
                    TextEdit {
                        start_byte: 0,
                        end_byte: 0,
                        inserted_text: "// 🦀\n".into(),
                    },
                    TextEdit {
                        start_byte: 3,
                        end_byte: 5,
                        inserted_text: "renamed".into(),
                    },
                ],
                SelectionState::caret(0),
            )
            .unwrap()
            .unwrap();
        editor.apply_transaction(transaction.clone()).unwrap();
        state.apply(&transaction).unwrap();
        assert_eq!(state.source, editor.text());
        let fresh = SyntaxState::new(
            DocumentId::new("fresh").unwrap(),
            Path::new("a.rs"),
            &editor.text(),
        );
        assert_eq!(state.spans, fresh.spans);
        assert_eq!(
            state.tree.as_ref().unwrap().root_node().to_sexp(),
            fresh.tree.as_ref().unwrap().root_node().to_sexp()
        );
        assert_eq!(
            state
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .named_child(91)
                .unwrap()
                .id(),
            unchanged_id,
            "a real unchanged subtree must survive incremental parsing"
        );
    }

    #[test]
    fn reused_capture_slices_match_fresh_whole_tree_queries() {
        for (path, source) in [
            (
                "a.rs",
                "//! doc\r\n#[derive(Debug)]\r\nstruct Café { n: u32 }\r\nmod m { pub fn f() { let s = r#\"世界\"#; foo::<u32>(); } }\r\nfn later() {}\r\n",
            ),
            (
                "Cargo.toml",
                "# header\r\n[package]\r\nname = \"世界\"\r\n[dependencies]\r\nthing = { version = \"1\", optional = true }\r\n",
            ),
        ] {
            let id = DocumentId::new("cached").unwrap();
            let mut state = SyntaxState::new(id.clone(), Path::new(path), source);
            let mut editor = EditorBuffer::new(id, source, NoopEditorEffects);
            for inserted in ["e\u{301}", "🦀", "\n", "\"", "/*", "\r\n", "界"] {
                let offset = editor.text().find("世界").unwrap();
                let transaction = editor
                    .preview_edits(
                        EditOrigin::Keyboard,
                        vec![TextEdit {
                            start_byte: offset as u64,
                            end_byte: offset as u64,
                            inserted_text: inserted.into(),
                        }],
                        SelectionState::caret(0),
                    )
                    .unwrap()
                    .unwrap();
                editor.apply_transaction(transaction.clone()).unwrap();
                state.apply(&transaction).unwrap();
                assert_whole_query(&mut state);
                let undo = editor.preview_undo().unwrap().unwrap();
                editor.apply_transaction(undo.clone()).unwrap();
                state.apply(&undo).unwrap();
                assert_whole_query(&mut state);
            }
        }
    }

    fn assert_whole_query(state: &mut SyntaxState) {
        let highlighter = state.highlighter.as_mut().unwrap();
        let fresh_tree = highlighter.parse(&state.source, None, None).unwrap();
        assert_eq!(
            state.spans,
            highlighter
                .spans(&state.source, fresh_tree.root_node(), None)
                .unwrap()
        );
        assert_eq!(
            state.tree.as_ref().unwrap().root_node().to_sexp(),
            fresh_tree.root_node().to_sexp()
        );
    }

    #[test]
    fn real_parser_cancellation_resets_before_another_document() {
        let mut highlighter = LanguageHighlighter::new(LanguageKind::Rust).unwrap();
        let source = "fn item() { let text = \"世界\"; }\n".repeat(5000);
        assert!(
            highlighter
                .parse(&source, None, Some(Instant::now()))
                .is_err()
        );
        let tree = highlighter.parse("fn recovered() {}", None, None).unwrap();
        assert!(!tree.root_node().has_error());
        assert_eq!(tree.root_node().end_byte(), "fn recovered() {}".len());
        assert!(
            highlighter
                .spans("fn recovered() {}", tree.root_node(), Some(Instant::now()))
                .is_err()
        );
    }

    #[test]
    fn parser_failure_effect_keeps_committed_version_and_drops_with_buffer() {
        let id = DocumentId::new("effect-failure").unwrap();
        let state = Rc::new(RefCell::new(SyntaxState::new(
            id.clone(),
            Path::new("a.rs"),
            "fn a() {}",
        )));
        let weak = Rc::downgrade(&state);
        state.borrow_mut().highlighter.as_mut().unwrap().parser = Parser::new();
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let sink = recorded.clone();
        let effects = SyntaxEffects::new(
            move |transaction: &EditorTransaction| {
                sink.borrow_mut().push(transaction.clone());
            },
            state.clone(),
        );
        let mut editor = EditorBuffer::new(id, "fn a() {}", effects);
        assert!(editor.insert_char(' ').unwrap());
        assert_eq!(recorded.borrow().len(), 1);
        assert_eq!(state.borrow().version, editor.version());
        assert_eq!(state.borrow().source, editor.text());
        assert!(state.borrow().spans.is_empty());
        assert!(state.borrow().tree.is_none());
        drop(state);
        drop(editor);
        assert!(
            weak.upgrade().is_none(),
            "closing must not retain syntax snapshots"
        );
    }

    #[test]
    fn expired_syntax_budget_falls_back_without_partial_captures_and_recovers() {
        let id = DocumentId::new("budget").unwrap();
        let mut state = SyntaxState::new(id, Path::new("a.rs"), "fn a() {}\nfn b() {}");
        assert!(!state.spans.is_empty());
        assert!(
            state
                .refresh_with_budget(std::time::Duration::ZERO)
                .is_err()
        );
        assert!(state.spans.is_empty());
        assert!(state.captures.is_empty());
        state
            .refresh_with_budget(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!state.spans.is_empty());
        assert_whole_query(&mut state);
    }

    #[test]
    fn parser_failure_clears_stale_spans_and_next_edit_can_recover() {
        let id = DocumentId::new("rust").unwrap();
        let mut state = SyntaxState::new(id.clone(), Path::new("a.rs"), "fn a() {}");
        let mut editor = EditorBuffer::new(id, "fn a() {}", NoopEditorEffects);
        // A parser without a language returns None, exercising actual parser failure.
        state.highlighter.as_mut().unwrap().parser = Parser::new();
        let transaction = editor
            .preview_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "// hi\n".into(),
                }],
                SelectionState::caret(0),
            )
            .unwrap()
            .unwrap();
        editor.apply_transaction(transaction.clone()).unwrap();
        assert!(state.apply(&transaction).is_err());
        assert!(state.spans.is_empty());
        assert!(state.tree.is_none());
        assert_eq!(state.version, editor.version());
        assert_eq!(state.source, editor.text());
        state.highlighter = Some(LanguageHighlighter::new(LanguageKind::Rust).unwrap());
        let transaction = editor
            .preview_edits(
                EditOrigin::Keyboard,
                vec![TextEdit {
                    start_byte: 0,
                    end_byte: 0,
                    inserted_text: "// recovered\n".into(),
                }],
                SelectionState::caret(0),
            )
            .unwrap()
            .unwrap();
        state.apply(&transaction).unwrap();
        assert!(!state.spans.is_empty());
    }
}
