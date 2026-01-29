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
