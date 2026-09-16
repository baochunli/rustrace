use rustrace_model::*;

#[test]
fn observation_and_recovery_have_stable_bounded_wire_contracts() {
    let event = Event::ExternalObservation(ExternalObservation {
        path: WorkspacePath::new("src/lib.rs").unwrap(),
        saved_hash: Some(observation_hash(b"A")),
        logical_hash: Some(observation_hash(b"B")),
        observed_hash: Some(observation_hash(b"C")),
        evidence_hash: Hash::from_bytes([7; 32]),
    });
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"type":"external_observation","payload":{"path":"src/lib.rs","saved_hash":"8ecba15e4b0f6199feb0f34ae04b65df88a46181c57099cd3232a0edfb96f452","logical_hash":"26aacfc748119ea5bf7a3b679c046370dc818aa194b3f629e08de4f18e960aa6","observed_hash":"5b105f11e7a2e4707a1dcddfbba5208ba4e00ac3cf8b90b871ccfc3a83bf2f71","evidence_hash":"0707070707070707070707070707070707070707070707070707070707070707"}}"#
    );
    let envelope = EventEnvelope {
        format_version: 1,
        session_id: SessionId::new("abc").unwrap(),
        sequence: 2,
        monotonic_millis: 17,
        wall_clock_utc: None,
        previous_event_hash: Hash::zero(),
        event_hash: Hash::zero(),
        event,
    }
    .seal(Hash::zero())
    .unwrap();
    let bytes = encode_envelope(&envelope).unwrap();
    assert_eq!(
        decode_envelope(&bytes, DecodePolicy::RejectUnsupported).unwrap(),
        DecodeOutcome::Decoded(envelope)
    );
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(
        "\"type\":\"external_observation\",\"payload\":{\"path\":\"src/lib.rs\",\"saved_hash\":"
    ));
    let decision = Event::RecoveryRecorded(RecoveryRecorded {
        evidence_hash: Hash::from_bytes([7; 32]),
        decision: RecoveryDecision::RestoreLogical,
    });
    assert_eq!(
        serde_json::to_string(&decision).unwrap(),
        r#"{"type":"recovery_recorded","payload":{"evidence_hash":"0707070707070707070707070707070707070707070707070707070707070707","decision":"restore_logical"}}"#
    );
}
