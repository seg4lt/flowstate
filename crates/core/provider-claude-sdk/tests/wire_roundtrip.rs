//! Phase 0.4 safety net: shape-check the wire contract between this
//! adapter and `provider-api`. If a field rename in `provider-api`
//! breaks the JSON encoding the frontend expects, this test fails at
//! `cargo test` time — before the drift ships.
//!
//! This is deliberately a wire-format check, not a behavior test. It
//! constructs a canonical `RuntimeEvent` that carries this adapter's
//! `ProviderKind`, round-trips it through JSON, and asserts that the
//! re-serialized value matches byte-for-byte (at the `serde_json::Value`
//! level).

use zenui_provider_api::{ProviderKind, RuntimeEvent, SessionStatus, SessionSummary};

#[test]
fn session_started_roundtrip() {
    let event = RuntimeEvent::SessionStarted {
        session: SessionSummary {
            session_id: "sess-claude-sdk-1".to_string(),
            provider: ProviderKind::Claude,
            status: SessionStatus::Ready,
            created_at: "2026-04-18T00:00:00Z".to_string(),
            updated_at: "2026-04-18T00:00:00Z".to_string(),
            turn_count: 0,
            model: Some("claude-sonnet-4-5-20250929".to_string()),
            project_id: Some("proj-1".to_string()),
        },
    };

    let encoded = serde_json::to_value(&event).expect("serialize");
    let decoded: RuntimeEvent = serde_json::from_value(encoded.clone()).expect("deserialize");
    let re_encoded = serde_json::to_value(&decoded).expect("re-serialize");

    assert_eq!(
        encoded, re_encoded,
        "RuntimeEvent::SessionStarted JSON shape is not stable across a round-trip"
    );

    // Sanity: the wire tag and ProviderKind tag are what the frontend
    // expects. If either of these changes, the frontend breaks silently.
    assert_eq!(encoded["type"], "session_started");
    assert_eq!(encoded["session"]["provider"], "claude");
}
