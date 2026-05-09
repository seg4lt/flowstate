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
//!     `{model: {id, providerID, modelID}, variant, parts}` returning 204
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

fn dummy_session(native_thread_id: Option<String>, cwd: &str) -> SessionDetail {
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
        // The session cwd is forwarded to opencode via the
        // `x-opencode-directory` header on every per-session request,
        // and opencode actually uses it (its tool subsystem changes
        // shell cwd to this path on the first tool invocation). Pick a
        // freshly-created scratch dir rather than the macOS tempdir
        // parent — opencode appears to scan the cwd at session-init
        // time, and a busy `/var/folders/.../T/` (thousands of
        // unrelated user files) caused the smoke test to hang.
        cwd: Some(cwd.to_string()),
    }
}

#[tokio::test]
#[ignore = "live: spawns real opencode; run with `-- --ignored`"]
async fn live_end_to_end_smoke() {
    if !is_opencode_available() {
        eprintln!("opencode binary not on PATH; skipping");
        return;
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let workdir = std::env::temp_dir().join(format!("zenui-opencode-live-{}", nanos));
    let session_cwd = std::env::temp_dir().join(format!("zenui-opencode-live-cwd-{}", nanos));
    std::fs::create_dir_all(&workdir).expect("workdir");
    std::fs::create_dir_all(&session_cwd).expect("session_cwd");
    let session_cwd_str = session_cwd.to_string_lossy().into_owned();
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
    let session = dummy_session(None, &session_cwd_str);
    let provider_state = adapter
        .start_session(&session)
        .await
        .expect("start_session");
    let native_id = provider_state
        .and_then(|s| s.native_thread_id)
        .expect("native id");

    let session = dummy_session(Some(native_id), &session_cwd_str);
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

/// Regression test for the "tools run in the wrong directory" bug.
///
/// The opencode adapter creates a native opencode session via
/// `POST /session` with a `directory` field; opencode persists that
/// directory and uses it as the cwd for every subsequent tool
/// invocation in that session. If the adapter ever silently falls
/// back to `self.working_directory` (= the daemon's app-data-dir)
/// when `session.cwd` is None, every bash / file tool runs in that
/// dir — which on macOS is `~/Library/Application Support/...`,
/// the smoking-gun we caught in production.
///
/// This test pins the contract end-to-end:
///   - construct a `SessionDetail` whose `cwd` is a freshly-created
///     tempdir
///   - run a turn that asks the model to call bash with `pwd`
///   - assert the assistant's streamed text contains the tempdir
///     path
///
/// Uses `PermissionMode::Bypass` so opencode auto-allows the bash
/// invocation — otherwise the turn would wedge on `permission.asked`
/// waiting for an answer that never comes (no UI in cargo-test).
#[tokio::test]
#[ignore = "live: spawns real opencode; run with `-- --ignored`"]
async fn live_session_cwd_is_honoured_by_bash_tool() {
    if !is_opencode_available() {
        eprintln!("opencode binary not on PATH; skipping");
        return;
    }

    // Adapter's working_directory = a separate scratch dir so we
    // can prove the *session* cwd (a different dir) is what
    // opencode actually uses, not the adapter's fallback.
    let adapter_workdir = std::env::temp_dir().join(format!(
        "zenui-opencode-adapter-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&adapter_workdir).expect("adapter workdir");
    let adapter = OpenCodeAdapter::new(adapter_workdir.clone());

    // The session cwd we want opencode tools to run in. Distinct
    // from `adapter_workdir` so a silent fallback to the adapter's
    // working_directory would produce a different `pwd` and the
    // assertion would catch it.
    let session_workdir = std::env::temp_dir().join(format!(
        "zenui-opencode-session-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&session_workdir).expect("session workdir");
    let session_workdir_str = session_workdir.to_string_lossy().into_owned();

    // Build a SessionDetail with the chosen cwd. Mimics what
    // RuntimeCore::resolve_session_cwd would produce after looking
    // up project.path.
    let now = "1970-01-01T00:00:00Z".to_string();
    let session = SessionDetail {
        summary: SessionSummary {
            session_id: "live-cwd-test".to_string(),
            provider: zenui_provider_api::ProviderKind::OpenCode,
            status: SessionStatus::Ready,
            created_at: now.clone(),
            updated_at: now,
            turn_count: 0,
            model: Some(TEST_MODEL.to_string()),
            project_id: None,
        },
        turns: Vec::new(),
        provider_state: None,
        cwd: Some(session_workdir_str.clone()),
    };

    let provider_state = adapter
        .start_session(&session)
        .await
        .expect("start_session should succeed with non-empty cwd");
    let native_id = provider_state
        .and_then(|s| s.native_thread_id)
        .expect("native id");

    // Re-build the session with the freshly-minted native id
    // alongside the same cwd so execute_turn reuses (no re-mint).
    let session = SessionDetail {
        cwd: Some(session_workdir_str.clone()),
        provider_state: Some(zenui_provider_api::ProviderSessionState {
            native_thread_id: Some(native_id),
            metadata: None,
        }),
        ..session
    };

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
            &UserInput::from_text(
                "Run `pwd` via the bash tool. Reply with only the absolute path.",
            ),
            // Bypass so opencode auto-allows the bash call.
            PermissionMode::Bypass,
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

    // Tool output is the most reliable signal — `pwd` writes the
    // absolute path. Also check assistant text for belt-and-braces.
    let mut tool_outputs: Vec<String> = Vec::new();
    let mut streamed_text = String::new();
    for ev in &events {
        match ev {
            ProviderTurnEvent::ToolCallCompleted { output, error: None, .. } => {
                tool_outputs.push(output.clone());
            }
            ProviderTurnEvent::AssistantTextDelta { delta } => {
                streamed_text.push_str(delta);
            }
            _ => {}
        }
    }

    let combined = format!("{} {}", tool_outputs.join(" "), streamed_text);
    assert!(
        combined.contains(&session_workdir_str),
        "expected `pwd` output to contain session_workdir `{session_workdir_str}`, \
         but it did not. tool_outputs={tool_outputs:?}, streamed={streamed_text:?}, \
         turn.output={:?}",
        turn.output
    );

    eprintln!(
        "live cwd OK: tools ran in `{session_workdir_str}` as expected. \
         {} events, {} tool outputs.",
        events.len(),
        tool_outputs.len()
    );
}
