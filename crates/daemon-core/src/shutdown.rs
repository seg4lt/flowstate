use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use zenui_runtime_core::RuntimeCore;

use crate::lifecycle::DaemonLifecycle;

/// Graceful daemon shutdown. Called by `run_blocking` when the shutdown
/// signal fires (idle timeout, SIGTERM, or explicit /api/shutdown POST).
///
/// Sequence:
/// 1. Ask the runtime to interrupt all in-flight turns, waiting up to
///    `grace` for the providers to settle.
/// 2. Return. The caller (`run_blocking`) then drops the local server
///    listener and, finally, the tokio runtime — reaping remaining
///    subprocess children.
///
/// Phase 1 implements step 1 by delegating to
/// `RuntimeCore::shutdown_all_turns`. Phase 2 wires lifecycle counters so
/// the idle watchdog can drive this path automatically.
pub async fn graceful_shutdown(
    runtime: Arc<RuntimeCore>,
    _lifecycle: Arc<DaemonLifecycle>,
    grace: Duration,
) -> Result<()> {
    let interrupted = runtime.shutdown_all_turns(grace).await;
    tracing::info!(
        interrupted,
        grace_ms = grace.as_millis() as u64,
        "graceful shutdown swept in-flight turns"
    );
    Ok(())
}
