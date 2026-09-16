use super::*;
use rustrace_editor::position::Utf16Position;

fn ready_protocol(now: Instant) -> Protocol {
    let mut protocol = Protocol::new(7, Path::new("/owned/work space"), now).unwrap();
    let initialize = protocol.outbox.pop_front().unwrap();
    protocol
        .handle(
            json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": {"capabilities": {
                    "positionEncoding": "utf-16",
                    "textDocumentSync": {"openClose": true, "change": 2}
                }}
            }),
            now,
        )
        .unwrap();
    protocol.outbox.clear();
    protocol
}

fn synced_protocol(now: Instant) -> (Protocol, DocumentState) {
    let mut protocol = ready_protocol(now);
    let document = DocumentState {
        document_id: DocumentId::new("document-main").unwrap(),
        path: WorkspacePath::new("src/main.rs").unwrap(),
        version: 4,
        text: "a🦀b".to_owned(),
    };
    protocol
        .reconcile_documents(std::slice::from_ref(&document))
        .unwrap();
    protocol.outbox.clear();
    (protocol, document)
}

fn request(protocol: &mut Protocol, document: &DocumentState, now: Instant) -> i64 {
    protocol
        .request_completion(
            document,
            5,
            Utf16Position {
                line: 0,
                character: 3,
            },
            23,
            now,
        )
        .unwrap()
        .unwrap()
}

fn reply(protocol: &mut Protocol, id: i64, result: Value, now: Instant) -> CompletionResponse {
    protocol
        .handle(json!({"jsonrpc": "2.0", "id": id, "result": result}), now)
        .unwrap();
    protocol.completions.pop_front().unwrap()
}

#[test]
fn completion_is_explicit_and_sends_the_exact_invoked_utf16_request() {
    let now = Instant::now();
    let (mut protocol, document) = synced_protocol(now);
    assert!(
        protocol.outbox.is_empty(),
        "sync alone must not request completion"
    );

    let id = request(&mut protocol, &document, now);
    let message = protocol.outbox.pop_front().unwrap();
    assert_eq!(message["id"], id);
    assert_eq!(message["method"], "textDocument/completion");
    assert_eq!(
        message["params"],
        json!({
            "textDocument": {"uri": "file:///owned/work%20space/src/main.rs"},
            "position": {"line": 0, "character": 3},
            "context": {"triggerKind": 1}
        })
    );
}

#[test]
fn completion_parses_bounded_plain_unicode_insertion_and_utf16_replacement() {
    let now = Instant::now();
    let (mut protocol, document) = synced_protocol(now);
    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    let response = reply(
        &mut protocol,
        id,
        json!({
            "isIncomplete": false,
            "items": [
                {"label": "東京", "kind": 6, "insertText": "東京", "insertTextFormat": 1},
                {"label": "replace crab", "kind": 3, "textEdit": {
                    "range": {"start": {"line": 0, "character": 1},
                              "end": {"line": 0, "character": 3}},
                    "newText": "λ"
                }}
            ]
        }),
        now,
    );
    assert_eq!(response.request.document_id, document.document_id);
    assert_eq!(response.request.request_sequence, 23);
    assert_eq!(response.request.version, 4);
    assert_eq!(response.request.position_byte, 5);
    assert_eq!(response.items.len(), 2);
    assert_eq!(response.items[0].label, "東京");
    assert_eq!(response.items[0].kind, Some(6));
    assert_eq!(
        response.items[0].edit,
        CompletionEdit::Insert("東京".into())
    );
    assert_eq!(
        response.items[1].edit,
        CompletionEdit::Replace {
            start: Utf16Position {
                line: 0,
                character: 1
            },
            end: Utf16Position {
                line: 0,
                character: 3
            },
            new_text: "λ".into(),
        }
    );
}

#[test]
fn every_unsupported_completion_class_rejects_the_whole_item() {
    let rejected = [
        json!({"label": "snippet", "insertText": "f(${1:x})", "insertTextFormat": 2}),
        json!({"label": "additional", "additionalTextEdits": []}),
        json!({"label": "command", "command": {"title": "run", "command": "x"}}),
        json!({"label": "insert-replace", "textEdit": {
            "insert": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "replace": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "newText": "x"
        }}),
        json!({"label": "cross-file", "textDocument": {"uri": "file:///other.rs"}}),
        json!({"label": "nested-cross-file", "textEdit": {
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "newText": "x", "uri": "file:///other.rs"
        }}),
        json!({"label": "workspace", "workspaceEdit": {"changes": {}}}),
        json!({"label": "ambiguous", "insertText": "x", "textEdit": {
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "newText": "y"
        }}),
    ];
    let now = Instant::now();
    for item in rejected {
        let (mut protocol, document) = synced_protocol(now);
        let id = request(&mut protocol, &document, now);
        protocol.outbox.clear();
        assert!(
            reply(&mut protocol, id, json!([item]), now)
                .items
                .is_empty()
        );
    }
}

#[test]
fn malformed_oversized_and_defaulted_results_are_inert_and_fixed_cap() {
    let now = Instant::now();
    for result in [
        json!({"isIncomplete": false}),
        json!({"items": [{"label": "x"}], "itemDefaults": {"insertTextFormat": 1}}),
        json!([{"kind": 3}]),
        json!([{"label": "x", "kind": 26}]),
        json!([{"label": "x", "textEdit": {"range": {
            "start": {"line": -1, "character": 0}, "end": {"line": 0, "character": 0}},
            "newText": "x"}}]),
        json!([{"label": "x".repeat(rustrace_model::MAX_STRING_BYTES + 1)}]),
    ] {
        let (mut protocol, document) = synced_protocol(now);
        let id = request(&mut protocol, &document, now);
        protocol.outbox.clear();
        assert!(reply(&mut protocol, id, result, now).items.is_empty());
    }

    let (mut protocol, document) = synced_protocol(now);
    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    let items = (0..MAX_COMPLETION_ITEMS + 5)
        .map(|index| json!({"label": format!("item-{index}")}))
        .collect::<Vec<_>>();
    assert_eq!(
        reply(&mut protocol, id, Value::Array(items), now)
            .items
            .len(),
        MAX_COMPLETION_ITEMS
    );
}

#[test]
fn completion_error_null_timeout_and_generation_change_only_retire_that_request() {
    let now = Instant::now();
    let (mut protocol, document) = synced_protocol(now);
    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -1, "message": "no"}}),
            now,
        )
        .unwrap();
    assert!(protocol.completions.pop_front().unwrap().items.is_empty());
    assert_eq!(protocol.state, ProtocolState::Ready);

    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    assert!(reply(&mut protocol, id, Value::Null, now).items.is_empty());

    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    protocol.check_deadlines(now + COMPLETION_DEADLINE).unwrap();
    assert!(!protocol.pending.contains_key(&id));
    assert!(protocol.completions.pop_front().unwrap().items.is_empty());
    assert_eq!(protocol.state, ProtocolState::Ready);

    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    protocol.generation += 1;
    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": id, "result": [{"label": "stale"}]}),
            now,
        )
        .unwrap();
    assert!(protocol.completions.is_empty());
}

#[test]
fn completion_response_at_deadline_is_empty_inert_and_cannot_revive() {
    let now = Instant::now();
    let (mut protocol, document) = synced_protocol(now);
    let id = request(&mut protocol, &document, now);
    protocol.outbox.clear();
    let deadline = now + COMPLETION_DEADLINE;

    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": id, "result": [{"label": "too late"}]}),
            deadline,
        )
        .unwrap();

    let response = protocol
        .completions
        .pop_front()
        .expect("an expired completion retires as an empty response");
    assert!(response.items.is_empty(), "deadline response stayed live");
    assert_eq!(response.request.request_sequence, 23);
    assert!(!protocol.pending.contains_key(&id));
    assert_eq!(protocol.state, ProtocolState::Ready);

    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": id, "result": [{"label": "revived"}]}),
            deadline + Duration::from_millis(1),
        )
        .unwrap();
    assert!(protocol.completions.is_empty());
    assert_eq!(protocol.state, ProtocolState::Ready);
}

#[test]
fn out_of_order_responses_preserve_each_bounded_request_identity() {
    let now = Instant::now();
    let (mut protocol, document) = synced_protocol(now);
    let older = request(&mut protocol, &document, now);
    let newer = protocol
        .request_completion(
            &document,
            5,
            Utf16Position {
                line: 0,
                character: 3,
            },
            24,
            now,
        )
        .unwrap()
        .unwrap();
    protocol.outbox.clear();
    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": newer, "result": [{"label": "new"}]}),
            now,
        )
        .unwrap();
    protocol
        .handle(
            json!({"jsonrpc": "2.0", "id": older, "result": [{"label": "old"}]}),
            now,
        )
        .unwrap();
    assert_eq!(protocol.completions.len(), 2);
    assert_eq!(
        protocol
            .completions
            .pop_front()
            .unwrap()
            .request
            .request_sequence,
        24
    );
    assert_eq!(
        protocol
            .completions
            .pop_front()
            .unwrap()
            .request
            .request_sequence,
        23
    );
}
