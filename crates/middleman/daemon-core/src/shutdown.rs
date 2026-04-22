use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zenui_provider_api::RuntimeEvent;
use zenui_runtime_core::RuntimeCore;

use crate::lifecycle::DaemonLifecycle;

/// Graceful daemon shutdown. Called by `run_blocking` when the shutdown
/// signal fires (idle timeout, SIGTERM, or explicit /api/shutdown POST).
///
/// Sequence:
/// 1. Publish `DaemonShuttingDown` so any attached clients can surface a
///    banner and stop issuing new commands.
/// 2. Ask the runtime to interrupt all in-flight turns, waiting up to
///    `grace` for the providers to settle.
/// 3. Return. The caller (`run_blocking`) then drops the local server
///    listener and, finally, the tokio runtime — reaping remaining
///    subprocess children.
pub async fn graceful_shutdown(
    runtime: Arc<RuntimeCore>,
    _lifecycle: Arc<DaemonLifecycle>,
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
    Ok(())
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
        Ok(())
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
        Ok(())
    }
}
