//! Single end-to-end live test against a real `opencode serve`
//! subprocess. **Ignored by default** because it spawns a process
//! and hits the live OpenCode Zen catalogue — expensive enough that
//! the unit tests carry most of the coverage and this only exists as
//! a final smoke check.
//!
//! To run:
//!
//! ```text
//! cargo test -p zenui-provider-opencode --test live_opencode \
//!     -- --ignored --nocapture
//! ```
//!
//! What this covers that the unit tests cannot:
//!   - real `opencode serve` readiness handshake (stdout banner
//!     detection, port allocation, password auth)
//!   - real `/app` health probe returning 200
//!   - real `POST /session` accepting our `permission` ruleset
//!   - real `POST /session/:id/prompt_async` with
//!     `{model: {providerID, modelID}, variant, parts}` returning 204
//!   - real SSE stream producing `message.part.delta` + `session.idle`
//!     at the shapes we parse
//!   - real `/config/providers` producing `cost` objects we can
//!     distinguish Zen-free from reflected-zero
//!
//! Everything else — event-dispatch edge cases, question round-trip,
//! permission/variant mapping, free-tag heuristic — is covered by
//! fast in-crate unit tests. See `src/events.rs::tests` and
//! `src/http.rs::tests`.
//!
//! Using `opencode/kimi-k2.5` (OpenCode Zen free tier) so runs don't
//! burn paid-model quota.
//!
//! **For the full API protocol reference** (all endpoints, SSE event
//! shapes, quirks, and how I captured them with the probe scripts
//! under `/tmp/opencode-probe/`), see `crates/core/provider-opencode/PROTOCOL.md`.

use std::time::Duration;

use tokio::sync::mpsc;
use zenui_provider_api::{
    PermissionMode, ProviderAdapter, ProviderStatusLevel, ProviderTurnEvent, SessionDetail,
    SessionStatus, SessionSummary, TurnEventSink, UserInput,
};
use zenui_provider_opencode::OpenCodeAdapter;

const TEST_MODEL: &str = "opencode/kimi-k2.5";
const TEST_PROMPT: &str = "Reply with the single word: hi.";

/// Ceiling for the one turn we send. Opencode Zen's kimi usually
/// responds in under 5s; we give 45s of headroom to absorb cold
/// starts and schedule variance before failing fast — a regression
/// that hangs the stream will still trip within a minute rather
/// than blocking CI indefinitely.
const TURN_TIMEOUT: Duration = Duration::from_secs(45);

fn is_opencode_available() -> bool {
    zenui_provider_api::find_cli_binary("opencode").is_some()
}

fn dummy_session(native_thread_id: Option<String>) -> SessionDetail {
    let now = "1970-01-01T00:00:00Z".to_string();
    let provider_state = native_thread_id.map(|id| zenui_provider_api::ProviderSessionState {
        native_thread_id: Some(id),
        metadata: None,
    });
    SessionDetail {
        summary: SessionSummary {
            session_id: "live-test".to_string(),
            provider: zenui_provider_api::ProviderKind::OpenCode,
            status: SessionStatus::Ready,
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            model: Some(TEST_MODEL.to_string()),
            project_id: None,
        },
        turns: Vec::new(),
        provider_state,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    }
}

#[tokio::test]
#[ignore = "live: spawns real opencode; run with `-- --ignored`"]
async fn live_end_to_end_smoke() {
    if !is_opencode_available() {
        eprintln!("opencode binary not on PATH; skipping");
        return;
    }

    let workdir = std::env::temp_dir().join(format!(
        "zenui-opencode-live-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&workdir).expect("workdir");
    let adapter = OpenCodeAdapter::new(workdir);

    // ── health ────────────────────────────────────────────────
    let status = adapter.health().await;
    assert!(
        matches!(status.status, ProviderStatusLevel::Ready),
        "expected health Ready, got {status:?}"
    );

    // ── free-tag regression: non-opencode providers must not be badged ──
    let models = adapter.fetch_models().await.expect("fetch_models");
    let non_opencode_free: Vec<&str> = models
        .iter()
        .filter(|m| m.is_free)
        .filter_map(|m| m.value.split_once('/').map(|(p, _)| p))
        .filter(|p| *p != "opencode")
        .collect();
    assert!(
        non_opencode_free.is_empty(),
        "free badge leaked to non-opencode providers: {non_opencode_free:?}"
    );

    // ── start_session + execute_turn ──────────────────────────
    let session = dummy_session(None);
    let provider_state = adapter
        .start_session(&session)
        .await
        .expect("start_session");
    let native_id = provider_state
        .and_then(|s| s.native_thread_id)
        .expect("native id");

    let session = dummy_session(Some(native_id));
    let (tx, mut rx) = mpsc::channel(256);
    let sink = TurnEventSink::new(tx);

    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    });

    let turn = tokio::time::timeout(
        TURN_TIMEOUT,
        adapter.execute_turn(
            &session,
            &UserInput::from_text(TEST_PROMPT),
            PermissionMode::Default,
            None,
            None,
            sink,
        ),
    )
    .await
    .expect("turn should finish under TURN_TIMEOUT")
    .expect("turn should succeed");

    drop(adapter);
    let events = collector.await.expect("collector");

    let deltas: Vec<String> = events
        .iter()
        .filter_map(|ev| match ev {
            ProviderTurnEvent::AssistantTextDelta { delta } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    let streamed = deltas.concat();
    assert!(
        !streamed.trim().is_empty(),
        "expected text deltas; got {} events / {} deltas / streamed={:?}",
        events.len(),
        deltas.len(),
        streamed
    );
    assert!(!turn.output.is_empty(), "ProviderTurnOutput.output empty");

    eprintln!(
        "live smoke OK: {} events, {} deltas, output = {:?}",
        events.len(),
        deltas.len(),
        turn.output
    );
}
