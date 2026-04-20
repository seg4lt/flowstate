//! End-to-end integration test for the checkpoint pipeline.
//!
//! Spins up a `RuntimeCore` with a real `FsCheckpointStore` backed by an
//! in-memory sqlite and a scratch data dir. Drives it through a full
//! capture → CheckpointCaptured event → rewind flow using a fake
//! provider adapter that simulates file edits on disk during
//! `execute_turn`. Exercises the same contract every provider adapter
//! relies on (capture happens at turn end; event fires once per
//! successful capture; rewind restores workspace files regardless of
//! how they got modified) without needing a real Claude / Codex /
//! Copilot runtime.
//!
//! If this suite is green, the per-provider smoke tests (deferred to
//! the integration-tests feature matrix) are testing that each real
//! adapter actually reaches `execute_turn` — not that the checkpoint
//! pipeline works.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::sync::broadcast::error::RecvError;
use zenui_checkpoints::FsCheckpointStore;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    ClientMessage, PermissionMode, ProviderAdapter, ProviderKind, ProviderStatus,
    ProviderStatusLevel, ProviderTurnOutput, ReasoningEffort, RewindOutcomeWire,
    RuntimeEvent, ServerMessage, SessionDetail, TurnEventSink, UserInput,
};
use zenui_runtime_core::{OrchestrationService, RuntimeCore};

/// Fake provider adapter. Each `execute_turn` call reads `next_edit`
/// from shared state and, if set, writes the given bytes to
/// `session.cwd/<rel_path>`. This simulates exactly the class of edit
/// (direct FS writes, no structured FileChange event emission) that
/// defeated the previous FileChangeRecord-based rewind — so the test
/// validates the specific failure mode zephyr was built to fix.
#[derive(Default)]
struct FsWritingAdapter {
    next_edit: tokio::sync::Mutex<Option<(String, Vec<u8>)>>,
}

impl FsWritingAdapter {
    fn new() -> Self {
        Self::default()
    }

    async fn queue_write(&self, rel_path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        *self.next_edit.lock().await = Some((rel_path.into(), bytes.into()));
    }
}

#[async_trait]
impl ProviderAdapter for FsWritingAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn health(&self) -> ProviderStatus {
        ProviderStatus {
            kind: ProviderKind::Codex,
            label: "Codex".to_string(),
            installed: true,
            authenticated: true,
            version: Some("fs-test".to_string()),
            status: ProviderStatusLevel::Ready,
            message: None,
            models: vec![],
            enabled: true,
            features: Default::default(),
        }
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        _permission_mode: PermissionMode,
        _reasoning_effort: Option<ReasoningEffort>,
        _thinking_mode: Option<zenui_provider_api::ThinkingMode>,
        _events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        // Pretend the agent ran a bash command or external tool: no
        // FileChange events, no tool-call events, just raw disk
        // writes. This is the scenario the old implementation couldn't
        // cover — we're explicitly validating that zephyr-based rewind
        // catches it.
        if let Some((rel, bytes)) = self.next_edit.lock().await.take() {
            let cwd = session
                .cwd
                .as_deref()
                .ok_or_else(|| "session has no cwd".to_string())?;
            let abs = Path::new(cwd).join(&rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create parent: {e}"))?;
            }
            std::fs::write(&abs, &bytes).map_err(|e| format!("write: {e}"))?;
        }
        Ok(ProviderTurnOutput {
            output: format!("ok: {}", input.text),
            provider_state: None,
        })
    }
}

struct Harness {
    runtime: Arc<RuntimeCore>,
    adapter: Arc<FsWritingAdapter>,
    workspace: TempDir,
    _data_dir: TempDir,
    _persistence_dir: TempDir,
}

fn setup() -> Harness {
    let data_dir = TempDir::new().unwrap();
    let persistence_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();

    let persistence = Arc::new(
        PersistenceService::new(persistence_dir.path().join("daemon.db"))
            .expect("open sqlite"),
    );
    let checkpoints = Arc::new(
        FsCheckpointStore::open(data_dir.path(), persistence.clone())
            .expect("open checkpoint store"),
    );

    let adapter = Arc::new(FsWritingAdapter::new());
    let runtime = Arc::new(RuntimeCore::new(
        vec![adapter.clone() as Arc<dyn ProviderAdapter>],
        Arc::new(OrchestrationService::new()),
        persistence,
        checkpoints,
        None,
        persistence_dir
            .path()
            .join("threads")
            .to_string_lossy()
            .into_owned(),
        "test-app".to_string(),
    ));

    Harness {
        runtime,
        adapter,
        workspace,
        _data_dir: data_dir,
        _persistence_dir: persistence_dir,
    }
}

async fn start_session_with_cwd(h: &Harness, cwd: &Path) -> String {
    // Projects anchor the session's cwd.
    let project = h
        .runtime
        .create_project_for_path(cwd.to_string_lossy().into_owned())
        .await
        .expect("create_project_for_path");
    let project_id = project.project_id;

    let response = h
        .runtime
        .handle_client_message(ClientMessage::StartSession {
            provider: ProviderKind::Codex,
            model: None,
            project_id: Some(project_id),
        })
        .await;
    match response {
        Some(ServerMessage::SessionCreated { session }) => session.session_id,
        other => panic!("expected SessionCreated, got {other:?}"),
    }
}

async fn send_turn_and_wait(
    h: &Harness,
    session_id: &str,
    input: &str,
) -> (String, Vec<RuntimeEvent>) {
    let mut rx = h.runtime.subscribe();
    h.runtime
        .handle_client_message(ClientMessage::SendTurn {
            session_id: session_id.to_string(),
            input: input.to_string(),
            images: vec![],
            permission_mode: None,
            reasoning_effort: None,
            thinking_mode: None,
        })
        .await;

    // Consume events until TurnCompleted for this session. Harvest
    // them so tests can assert on CheckpointCaptured.
    let mut events = Vec::new();
    let mut turn_id = String::new();
    loop {
        match rx.recv().await {
            Ok(event) => {
                if let RuntimeEvent::TurnCompleted {
                    ref turn,
                    session_id: ref sid,
                    ..
                } = event
                {
                    if sid == session_id {
                        turn_id = turn.turn_id.clone();
                        events.push(event);
                        break;
                    }
                }
                events.push(event);
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }
    (turn_id, events)
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, bytes).unwrap();
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap()
}

#[tokio::test]
async fn capture_fires_event_and_rewind_restores_bash_driven_edit() {
    let h = setup();

    // Seed the workspace before the session starts so it's part of
    // the first checkpoint's baseline.
    write(h.workspace.path(), "a.rs", b"original");

    let session_id = start_session_with_cwd(&h, h.workspace.path()).await;

    // Turn 1: adapter writes "v2" to a.rs with no structured event,
    // simulating the agent running a bash command. Capture at turn
    // end should pick this up via the stat-walk + hash path.
    h.adapter.queue_write("a.rs", b"v2".to_vec()).await;
    let (turn1_id, events_turn1) = send_turn_and_wait(&h, &session_id, "msg 1").await;
    assert!(!turn1_id.is_empty());
    assert_eq!(read(h.workspace.path(), "a.rs"), b"v2");

    // CheckpointCaptured must fire for this turn.
    let captured_turn1 = events_turn1.iter().any(|e| {
        matches!(
            e,
            RuntimeEvent::CheckpointCaptured {
                session_id: sid,
                turn_id: tid,
            } if sid == &session_id && tid == &turn1_id
        )
    });
    assert!(
        captured_turn1,
        "expected CheckpointCaptured event for turn 1",
    );

    // Turn 2: write a third revision so we have two post-baseline
    // changes to roll back.
    h.adapter.queue_write("a.rs", b"v3".to_vec()).await;
    let (turn2_id, _events_turn2) = send_turn_and_wait(&h, &session_id, "msg 2").await;
    assert_eq!(read(h.workspace.path(), "a.rs"), b"v3");

    // Rewind to turn 2's pre-state (i.e. undo turn 2 onwards). The
    // a.rs we'll see restored is "v2" — the state after turn 1 but
    // before turn 2.
    let outcome = {
        let response = h
            .runtime
            .handle_client_message(ClientMessage::RewindFiles {
                session_id: session_id.clone(),
                turn_id: turn2_id.clone(),
                dry_run: false,
                confirm_conflicts: false,
            })
            .await;
        match response {
            Some(ServerMessage::RewindFilesResult { outcome, .. }) => outcome,
            other => panic!("expected RewindFilesResult, got {other:?}"),
        }
    };
    match outcome {
        RewindOutcomeWire::Applied { paths_restored, .. } => {
            assert!(
                paths_restored.contains(&"a.rs".to_string()),
                "expected a.rs to be restored, got {paths_restored:?}",
            );
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(read(h.workspace.path(), "a.rs"), b"v2");
}

#[tokio::test]
async fn rewind_returns_unavailable_for_missing_turn() {
    // Sanity: a rewind request naming a turn id that was never
    // captured (e.g. capture failed silently, session predates the
    // feature) comes back as NoCheckpoint so the client can show the
    // "rewind unavailable" copy instead of a generic error.
    let h = setup();
    write(h.workspace.path(), "a.rs", b"v1");
    let session_id = start_session_with_cwd(&h, h.workspace.path()).await;

    let response = h
        .runtime
        .handle_client_message(ClientMessage::RewindFiles {
            session_id: session_id.clone(),
            turn_id: "turn-that-never-existed".to_string(),
            dry_run: false,
            confirm_conflicts: false,
        })
        .await;
    match response {
        Some(ServerMessage::RewindFilesResult {
            outcome: RewindOutcomeWire::Unavailable {
                reason: zenui_provider_api::RewindUnavailableReason::NoCheckpoint,
            },
            ..
        }) => {}
        other => panic!("expected NoCheckpoint Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn rewind_returns_unavailable_for_disabled_checkpoints() {
    let h = setup();
    write(h.workspace.path(), "a.rs", b"v1");
    let session_id = start_session_with_cwd(&h, h.workspace.path()).await;

    // Flip global off BEFORE the turn. Capture should be skipped.
    h.runtime
        .handle_client_message(ClientMessage::SetCheckpointsEnabled { enabled: false })
        .await;

    h.adapter.queue_write("a.rs", b"v2".to_vec()).await;
    let (turn_id, events) = send_turn_and_wait(&h, &session_id, "m").await;
    assert_eq!(read(h.workspace.path(), "a.rs"), b"v2");

    // No CheckpointCaptured event for this turn.
    let any_captured = events.iter().any(|e| {
        matches!(e, RuntimeEvent::CheckpointCaptured { turn_id: tid, .. } if tid == &turn_id)
    });
    assert!(
        !any_captured,
        "CheckpointCaptured should not fire when checkpoints are disabled",
    );

    // Rewind must surface Disabled — not NoCheckpoint — because the
    // UX needs to tell the user this is a setting, not a missing
    // snapshot.
    let response = h
        .runtime
        .handle_client_message(ClientMessage::RewindFiles {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            dry_run: false,
            confirm_conflicts: false,
        })
        .await;
    match response {
        Some(ServerMessage::RewindFilesResult {
            outcome: RewindOutcomeWire::Unavailable { reason },
            ..
        }) => {
            assert_eq!(reason, zenui_provider_api::RewindUnavailableReason::Disabled);
        }
        other => panic!("expected Unavailable Disabled, got {other:?}"),
    }
}
