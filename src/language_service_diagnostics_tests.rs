use super::*;

fn protocol(now: Instant) -> Protocol {
    Protocol::new(9, Path::new("/owned/work space"), now).unwrap()
}

fn initialize(protocol: &mut Protocol, now: Instant) {
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
}

fn document(index: u64, version: u64, text: &str) -> DocumentState {
    DocumentState {
        document_id: DocumentId::new(format!("document-{index}")).unwrap(),
        path: WorkspacePath::new(format!("src/file-{index}.rs")).unwrap(),
        version,
        text: text.to_owned(),
    }
}

fn notification(uri: &str, version: Option<u64>, diagnostics: Value) -> Value {
    let mut params = serde_json::Map::from_iter([
        ("uri".to_owned(), Value::String(uri.to_owned())),
        ("diagnostics".to_owned(), diagnostics),
    ]);
    if let Some(version) = version {
        params.insert("version".to_owned(), json!(version));
    }
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": params,
    })
}

fn diagnostic(message: &str, severity: u8) -> Value {
    json!({
        "range": {
            "start": {"line": 0, "character": 1},
            "end": {"line": 0, "character": 3}
        },
        "severity": severity,
        "message": message
    })
}

#[test]
fn publish_diagnostics_accepts_only_the_exact_synced_document_version() {
    let now = Instant::now();
    let mut protocol = protocol(now);
    initialize(&mut protocol, now);
    let current = document(1, 7, "a😀\n");
    protocol
        .reconcile_documents(std::slice::from_ref(&current))
        .unwrap();
    protocol.outbox.clear();
    let uri = document_uri(&protocol.root_uri, &current.path);

    for version in [None, Some(6), Some(8)] {
        protocol
            .handle(
                notification(&uri, version, json!([diagnostic("stale", 1)])),
                now,
            )
            .unwrap();
    }
    protocol
        .handle(
            notification(&uri, Some(7), json!([diagnostic("fresh", 1)])),
            now,
        )
        .unwrap();

    assert_eq!(protocol.diagnostics.len(), 1);
    let published = protocol.diagnostics.pop_front().unwrap();
    assert_eq!(published.generation, 9);
    assert_eq!(published.document_id, current.document_id);
    assert_eq!(published.path, current.path);
    assert_eq!(published.version, 7);
    assert_eq!(published.diagnostics[0].message, "fresh");
}

#[test]
fn publish_diagnostics_bounds_count_and_safe_display_message() {
    let now = Instant::now();
    let mut protocol = protocol(now);
    initialize(&mut protocol, now);
    let current = document(1, 1, "abc\n");
    protocol
        .reconcile_documents(std::slice::from_ref(&current))
        .unwrap();
    protocol.outbox.clear();
    let uri = document_uri(&protocol.root_uri, &current.path);
    let hostile = format!("bad\u{1b}[2J\n{}", "x".repeat(8_000));
    let diagnostics = (0..MAX_LIVE_DIAGNOSTICS_PER_DOCUMENT + 5)
        .map(|_| diagnostic(&hostile, 2))
        .collect::<Vec<_>>();

    protocol
        .handle(notification(&uri, Some(1), Value::Array(diagnostics)), now)
        .unwrap();

    let published = protocol.diagnostics.pop_front().unwrap();
    assert_eq!(
        published.diagnostics.len(),
        MAX_LIVE_DIAGNOSTICS_PER_DOCUMENT
    );
    assert!(published.diagnostics[0].message.len() <= MAX_LIVE_DIAGNOSTIC_MESSAGE_BYTES);
    assert!(published.diagnostics[0].message.contains("\\u{1b}"));
    assert!(published.diagnostics[0].message.contains("\\n"));
    assert!(
        !published.diagnostics[0]
            .message
            .chars()
            .any(char::is_control)
    );
}

#[test]
fn malformed_or_unowned_publish_diagnostics_are_inert_and_logging_is_bounded() {
    let now = Instant::now();
    let mut protocol = protocol(now);
    initialize(&mut protocol, now);
    let current = document(1, 1, "abc\n");
    protocol
        .reconcile_documents(std::slice::from_ref(&current))
        .unwrap();
    protocol.outbox.clear();
    let uri = document_uri(&protocol.root_uri, &current.path);

    let malformed = [
        json!({"jsonrpc":"2.0", "method":"textDocument/publishDiagnostics"}),
        notification(&uri, Some(1), json!({})),
        notification(&uri, Some(1), json!([{"message":"missing range"}])),
        notification(&uri, Some(1), json!([diagnostic("bad severity", 9)])),
        notification(
            "file:///outside.rs",
            Some(1),
            json!([diagnostic("outside", 1)]),
        ),
    ];
    for _ in 0..4 {
        for message in &malformed {
            protocol.handle(message.clone(), now).unwrap();
        }
    }

    assert!(protocol.diagnostics.is_empty());
    assert_eq!(protocol.diagnostic_drop_logs, MAX_DIAGNOSTIC_DROP_LOGS);
}

#[test]
fn document_replacement_retires_queued_live_diagnostics() {
    let now = Instant::now();
    let mut protocol = protocol(now);
    initialize(&mut protocol, now);
    let old = document(1, 1, "old");
    protocol
        .reconcile_documents(std::slice::from_ref(&old))
        .unwrap();
    protocol.outbox.clear();
    let uri = document_uri(&protocol.root_uri, &old.path);
    protocol
        .handle(
            notification(&uri, Some(1), json!([diagnostic("old", 1)])),
            now,
        )
        .unwrap();
    assert_eq!(protocol.diagnostics.len(), 1);

    let mut replacement = document(2, 0, "new");
    replacement.path = old.path;
    protocol.reconcile_documents(&[replacement]).unwrap();

    assert!(protocol.diagnostics.is_empty());
}
