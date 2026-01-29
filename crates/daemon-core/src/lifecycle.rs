use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, oneshot};
use zenui_http_api::{ConnectionObserver, DaemonStatus};
use zenui_runtime_core::TurnLifecycleObserver;

/// Runtime state driving the daemon's idle-shutdown behavior.
///
/// Two counters (`connected_clients` and `in_flight_turns`) gate whether
/// the daemon is "idle." A single `activity_notify` is tripped on every
/// counter change so the idle watchdog can promptly re-evaluate.
///
/// Phase 1 defines the struct. Phase 2 wires it to `http-api` (connection
/// hooks) and `runtime-core` (via `TurnLifecycleObserver`), and the idle
/// watchdog is started from `run_blocking`.
#[derive(Debug)]
pub struct DaemonLifecycle {
    connected_clients: AtomicUsize,
    in_flight_turns: AtomicUsize,
    activity_notify: Notify,
    shutdown_signal: Notify,
    idle_timeout: Duration,
    started_at: Instant,
    started_at_rfc3339: String,
    daemon_version: String,
}

impl DaemonLifecycle {
    pub fn new(idle_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            connected_clients: AtomicUsize::new(0),
            in_flight_turns: AtomicUsize::new(0),
            activity_notify: Notify::new(),
            shutdown_signal: Notify::new(),
            idle_timeout,
            started_at: Instant::now(),
            started_at_rfc3339: chrono::Utc::now().to_rfc3339(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    pub fn connected_clients(&self) -> usize {
        self.connected_clients.load(Ordering::Relaxed)
    }

    pub fn in_flight_turns(&self) -> usize {
        self.in_flight_turns.load(Ordering::Relaxed)
    }

    pub fn client_connected(&self) {
        self.connected_clients.fetch_add(1, Ordering::Relaxed);
        self.activity_notify.notify_waiters();
    }

    pub fn client_disconnected(&self) {
        self.connected_clients.fetch_sub(1, Ordering::Relaxed);
        self.activity_notify.notify_waiters();
    }

    pub fn turn_started(&self) {
        self.in_flight_turns.fetch_add(1, Ordering::Relaxed);
        self.activity_notify.notify_waiters();
    }

    pub fn turn_ended(&self) {
        self.in_flight_turns.fetch_sub(1, Ordering::Relaxed);
        self.activity_notify.notify_waiters();
    }

    pub fn request_shutdown(&self) {
        self.shutdown_signal.notify_waiters();
    }

    pub async fn wait_for_shutdown(&self) {
        self.shutdown_signal.notified().await
    }

    pub fn is_idle(&self) -> bool {
        self.connected_clients() == 0 && self.in_flight_turns() == 0
    }
}

impl TurnLifecycleObserver for DaemonLifecycle {
    fn on_turn_start(&self, _session_id: &str) {
        self.turn_started();
    }

    fn on_turn_end(&self, _session_id: &str) {
        self.turn_ended();
    }
}

impl ConnectionObserver for DaemonLifecycle {
    fn on_client_connected(&self) {
        self.client_connected();
    }

    fn on_client_disconnected(&self) {
        self.client_disconnected();
    }

    fn on_shutdown_requested(&self) {
        self.request_shutdown();
    }

    fn status(&self) -> Option<DaemonStatus> {
        Some(DaemonStatus {
            connected_clients: self.connected_clients(),
            in_flight_turns: self.in_flight_turns(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            daemon_version: self.daemon_version.clone(),
            started_at: self.started_at_rfc3339.clone(),
        })
    }
}

/// Idle watchdog task: waits for both counters to reach zero, starts the
/// idle timer, and fires `shutdown_tx` when the timer elapses without any
/// intervening activity. Exits immediately on explicit shutdown request.
pub async fn idle_watchdog(
    lifecycle: Arc<DaemonLifecycle>,
    shutdown_tx: oneshot::Sender<IdleShutdownReason>,
) {
    let mut pending_tx = Some(shutdown_tx);
    loop {
        // Wait for both counters to reach zero. Break on explicit shutdown.
        while !lifecycle.is_idle() {
            tokio::select! {
                _ = lifecycle.activity_notify.notified() => {}
                _ = lifecycle.shutdown_signal.notified() => {
                    if let Some(tx) = pending_tx.take() {
                        let _ = tx.send(IdleShutdownReason::Explicit);
                    }
                    return;
                }
            }
        }

        // Both zero. Race the idle timer against new activity or explicit stop.
        tokio::select! {
            _ = tokio::time::sleep(lifecycle.idle_timeout) => {
                tracing::info!(
                    idle_timeout = ?lifecycle.idle_timeout,
                    "daemon idle timer elapsed; requesting shutdown"
                );
                if let Some(tx) = pending_tx.take() {
                    let _ = tx.send(IdleShutdownReason::Idle);
                }
                return;
            }
            _ = lifecycle.activity_notify.notified() => {
                // Activity before the timer expired; re-check from the top.
                continue;
            }
            _ = lifecycle.shutdown_signal.notified() => {
                if let Some(tx) = pending_tx.take() {
                    let _ = tx.send(IdleShutdownReason::Explicit);
                }
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleShutdownReason {
    Idle,
    Explicit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_watchdog_fires_on_idle_timeout() {
        let lifecycle = DaemonLifecycle::new(Duration::from_millis(50));
        let (tx, rx) = oneshot::channel();
        tokio::spawn(idle_watchdog(lifecycle.clone(), tx));

        let reason = tokio::time::timeout(Duration::from_millis(500), rx)
            .await
            .expect("watchdog should fire within 500ms")
            .expect("sender should send");
        assert_eq!(reason, IdleShutdownReason::Idle);
    }

    #[tokio::test]
    async fn idle_watchdog_waits_on_client_and_turn() {
        let lifecycle = DaemonLifecycle::new(Duration::from_millis(50));
        lifecycle.client_connected();
        lifecycle.turn_started();

        let (tx, rx) = oneshot::channel();
        tokio::spawn(idle_watchdog(lifecycle.clone(), tx));

        // With client+turn both active, watchdog should not fire.
        let result = tokio::time::timeout(Duration::from_millis(200), rx).await;
        assert!(result.is_err(), "watchdog fired too early: {:?}", result);
    }

    #[tokio::test]
    async fn idle_watchdog_fires_on_explicit_request() {
        let lifecycle = DaemonLifecycle::new(Duration::from_secs(3600));
        lifecycle.client_connected();

        let (tx, rx) = oneshot::channel();
        tokio::spawn(idle_watchdog(lifecycle.clone(), tx));

        tokio::time::sleep(Duration::from_millis(20)).await;
        lifecycle.request_shutdown();

        let reason = tokio::time::timeout(Duration::from_millis(500), rx)
            .await
            .expect("watchdog should fire within 500ms on explicit shutdown")
            .expect("sender should send");
        assert_eq!(reason, IdleShutdownReason::Explicit);
    }
}
