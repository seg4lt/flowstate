//! Phase 0.4 safety net: shape-check the wire contract between this
//! adapter and `provider-api`. See provider-claude-sdk for the full
//! rationale; each adapter gets a near-identical test so that a
//! `ProviderKind` tag rename is caught per-adapter.

use zenui_provider_api::{ProviderKind, RuntimeEvent, SessionStatus, SessionSummary};

#[test]
fn session_started_roundtrip() {
    let event = RuntimeEvent::SessionStarted {
        session: SessionSummary {
            session_id: "sess-claude-cli-1".to_string(),
            provider: ProviderKind::ClaudeCli,
            status: SessionStatus::Ready,
            created_at: "2026-04-18T00:00:00Z".to_string(),
            updated_at: "2026-04-18T00:00:00Z".to_string(),
            turn_count: 0,
            model: Some("claude-sonnet-4-5".to_string()),
            project_id: None,
        },
    };

    let encoded = serde_json::to_value(&event).expect("serialize");
    let decoded: RuntimeEvent = serde_json::from_value(encoded.clone()).expect("deserialize");
    let re_encoded = serde_json::to_value(&decoded).expect("re-serialize");

    assert_eq!(
        encoded, re_encoded,
        "RuntimeEvent::SessionStarted JSON shape is not stable across a round-trip"
    );
    assert_eq!(encoded["type"], "session_started");
    assert_eq!(encoded["session"]["provider"], "claude_cli");
}
