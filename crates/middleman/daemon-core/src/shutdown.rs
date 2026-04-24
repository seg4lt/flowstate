use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
use zenui_runtime_core::RuntimeCore;

use crate::lifecycle::DaemonLifecycle;

/// Outer per-adapter timeout applied around each
/// `ProviderAdapter::shutdown` call. Individual implementations are
/// expected to be bounded internally (e.g. opencode's SIGTERM → 3s
/// wait → SIGKILL escalation tops out around 5s), but we wrap the
/// call in a timeout anyway so one wedged adapter can't block
/// shutdown of its siblings.
const ADAPTER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(8);

/// Graceful daemon shutdown. Called by `run_blocking` when the shutdown
/// signal fires (idle timeout, SIGTERM, or explicit /api/shutdown POST).
///
/// Sequence:
/// 1. Publish `DaemonShuttingDown` so any attached clients can surface a
///    banner and stop issuing new commands.
/// 2. Ask the runtime to interrupt all in-flight turns, waiting up to
///    `grace` for the providers to settle.
/// 3. Explicitly call `ProviderAdapter::shutdown` on each adapter with
///    a bounded per-adapter timeout. This replaces the previous
///    implicit reliance on `Drop` firing at end-of-scope to tear down
///    subprocesses — that path is racy (lingering `Arc`s past scope
///    end, async teardown work can't run from `Drop`) and was the
///    root cause of orphaned `opencode serve` + per-session CLI
///    children after a host-process close.
/// 4. Return. The caller (`run_blocking` / the Tauri shell's daemon
///    task) then drops the local server listener and tokio runtime.
pub async fn graceful_shutdown(
    runtime: Arc<RuntimeCore>,
    _lifecycle: Arc<DaemonLifecycle>,
    adapters: &[Arc<dyn ProviderAdapter>],
    grace: Duration,
) -> Result<()> {
    runtime.publish(RuntimeEvent::DaemonShuttingDown {
        reason: "daemon graceful shutdown in progress".to_string(),
    });
    let interrupted = runtime.shutdown_all_turns(grace).await;
    tracing::info!(
        interrupted,
        grace_ms = grace.as_millis() as u64,
        "graceful shutdown swept in-flight turns"
    );

    shutdown_adapters(adapters).await;
    Ok(())
}

/// Shared adapter-sweep implementation used by both shutdown paths.
/// Kept private (only the two `pub async fn`s below invoke it) so the
/// per-adapter timeout and logging stay in one place.
async fn shutdown_adapters(adapters: &[Arc<dyn ProviderAdapter>]) {
    for adapter in adapters {
        let kind = adapter.kind();
        match tokio::time::timeout(ADAPTER_SHUTDOWN_TIMEOUT, adapter.shutdown()).await {
            Ok(()) => {
                tracing::info!(?kind, "adapter shutdown complete");
            }
            Err(_) => {
                tracing::warn!(
                    ?kind,
                    timeout_ms = ADAPTER_SHUTDOWN_TIMEOUT.as_millis() as u64,
                    "adapter shutdown timed out; proceeding — any surviving children will be reaped by the startup orphan scan on next launch"
                );
            }
        }
    }
}

/// Wait for in-flight turns to finish naturally, then tear down.
///
/// Phase 5.5.3 addition. `graceful_shutdown` above INTERRUPTS turns
/// with a grace period — the right behaviour for idle-timeout / SIGINT
/// flows where the user wants to stop now. `drain_shutdown` is the
/// complement for the "UI closed but the daemon should let the
/// current turn finish" path that Phase 6 enables: poll
/// `lifecycle.is_quiescent()` until it holds (turns naturally
/// completed) or `max_wait` elapses, THEN fall back to interrupt.
///
/// Call sites pick between the two:
/// - Window close in the Tauri shell (post-Phase-6) → drain, so a
///   mid-turn close doesn't visibly cancel the user's request.
/// - SIGINT / Ctrl-C → interrupt (the user asked for "stop now").
/// - Idle watchdog → interrupt (no turns are running by definition,
///   so drain would be a no-op anyway).
///
/// Emits a `DaemonShuttingDown` event at the start of the drain so
/// attached clients can surface a banner; the UI decides whether to
/// show a progress indicator or a spinner.
pub async fn drain_shutdown(
    runtime: Arc<RuntimeCore>,
    lifecycle: Arc<DaemonLifecycle>,
    adapters: &[Arc<dyn ProviderAdapter>],
    max_wait: Duration,
    interrupt_grace: Duration,
) -> Result<()> {
    runtime.publish(RuntimeEvent::DaemonShuttingDown {
        reason: "daemon drain shutdown in progress".to_string(),
    });
    let drained = lifecycle.wait_for_quiescent(max_wait).await;
    if drained {
        tracing::info!(
            max_wait_ms = max_wait.as_millis() as u64,
            "drain shutdown: all in-flight turns completed naturally"
        );
    } else {
        // Timeout elapsed; some turns are still running. Escalate to
        // interrupt with the caller's configured grace so we don't
        // leave agent subprocesses hanging.
        let remaining = lifecycle.in_flight_turns();
        tracing::info!(
            remaining,
            max_wait_ms = max_wait.as_millis() as u64,
            interrupt_grace_ms = interrupt_grace.as_millis() as u64,
            "drain shutdown: max_wait elapsed; escalating to interrupt"
        );
        let interrupted = runtime.shutdown_all_turns(interrupt_grace).await;
        tracing::info!(
            interrupted,
            "drain shutdown: interrupt phase completed"
        );
    }
    // Always sweep adapters after the turns settle (either naturally
    // or via interrupt). Matches the explicit-kill contract in
    // `graceful_shutdown` above — no path through drain_shutdown
    // should leave a provider subprocess running.
    shutdown_adapters(adapters).await;
    Ok(())
}
