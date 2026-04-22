//! Shared loopback-HTTP handle used by provider adapters to wire
//! per-session MCP subprocesses back to the running runtime.
//!
//! # Why this exists
//!
//! Every non-Claude-SDK provider (opencode, Copilot SDK, Claude CLI,
//! Codex, Copilot CLI) exposes orchestration tools to its agent by
//! spawning `flowstate mcp-server --http-base URL --session-id SID`
//! as an MCP stdio subprocess. That subprocess needs:
//!
//! 1. The loopback HTTP base URL the runtime is listening on.
//! 2. The absolute path to the flowstate binary itself (provider
//!    config files that accept a static `command` array — e.g.
//!    `opencode.json`, `.mcp.json` — cannot use `$PATH` lookups).
//!
//! Both are known only *after* the runtime has bound its loopback
//! listener (the port is ephemeral) but adapters are constructed
//! *before* the runtime finishes bootstrapping. This handle bridges
//! that temporal gap: the app creates an [`OrchestrationIpcHandle`]
//! up front, clones it into every adapter that needs it, then calls
//! [`OrchestrationIpcHandle::set`] once the loopback transport is
//! live. Adapters read back via [`OrchestrationIpcHandle::get`] at
//! session-spawn time.
//!
//! # Why a `watch::Receiver`, not a `OnceCell`
//!
//! Phase 5.5 of the architecture plan: the future `flowstate-daemon`
//! process (Phase 6) can be respawned by the Tauri shell after a
//! crash. A respawned daemon binds a new ephemeral loopback port, so
//! the `IpcInfo` *must* be mutable across the lifetime of a long-
//! running adapter. With a `OnceCell` the first-populated value
//! stuck forever — adapters would hand stale URLs to new sessions
//! after respawn. The `tokio::sync::watch` channel gives us:
//!
//! - Atomic publish of a new `Option<IpcInfo>` when the daemon
//!   rebinds or shuts down.
//! - Cheap cloneable `Receiver`s for every adapter.
//! - A `borrow()` path that's allocation-free on the hot
//!   session-spawn route.
//!
//! Already-spawned MCP subprocesses are unaffected — the old URL
//! only matters for dispatch, and the per-session watchdog (see
//! `crates/core/mcp-server/src/lib.rs::spawn_parent_watchdog`) kills
//! those subprocesses when their parent flowstate dies anyway.
//!
//! # No bearer-token auth on the loopback
//!
//! The transport binds `127.0.0.1` only, so non-loopback peers
//! cannot reach it. On a single-user desktop the only processes that
//! can connect are the user's own — which already have unrestricted
//! access to the flowstate SQLite store, session attachments, and
//! `~/.claude.json`, so a bearer token on the loopback HTTP would
//! be theater. We rely entirely on the loopback bind plus the OS's
//! process-level isolation. If flowstate is ever deployed on a
//! multi-user host, reintroduce `HttpTransport::new_with_auth` and
//! a token field here.

use std::path::PathBuf;

use tokio::sync::watch;

/// Concrete values a provider adapter needs to spawn an MCP
/// subprocess: the loopback URL to call and the absolute path to the
/// `flowstate` binary (so MCP config files can embed the full
/// command-line). `executable_path` should resolve to the same
/// binary that's serving the HTTP transport — both the Tauri app
/// and any future standalone daemon populate this from
/// `std::env::current_exe()`.
#[derive(Debug, Clone)]
pub struct OrchestrationIpcInfo {
    pub base_url: String,
    pub executable_path: PathBuf,
}

/// Cheap-to-clone handle over a possibly-populated
/// [`OrchestrationIpcInfo`]. Adapters keep a clone; the embedder
/// populates it after the loopback transport binds and re-publishes
/// on daemon restart. Failures to populate are logged at the
/// embedder, not surfaced here — adapters either read `Some(info)`
/// and wire orchestration or read `None` and skip it.
#[derive(Clone, Debug)]
pub struct OrchestrationIpcHandle {
    rx: watch::Receiver<Option<OrchestrationIpcInfo>>,
    tx: watch::Sender<Option<OrchestrationIpcInfo>>,
}

impl Default for OrchestrationIpcHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl OrchestrationIpcHandle {
    /// Create a fresh handle. The channel seed is `None`; callers
    /// populate it via [`publish`](Self::publish) once the loopback
    /// HTTP listener is live.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { rx, tx }
    }

    /// Publish a new `IpcInfo`. Overwrites whatever value was there.
    ///
    /// First call populates a previously-empty channel; subsequent
    /// calls (daemon respawn, restart-after-crash) replace the
    /// old value so new sessions pick up the current URL. Returns
    /// `true` iff at least one receiver is still attached — embedders
    /// can use this to detect "no one's listening, no point updating"
    /// races, but none of today's adapter callers care.
    pub fn publish(&self, info: OrchestrationIpcInfo) -> bool {
        self.tx.send(Some(info)).is_ok()
    }

    /// Publish `None` to signal "loopback is down; dispatch tools
    /// unavailable until the next `publish`". Used by the daemon
    /// shutdown path and by respawn bookkeeping.
    pub fn clear(&self) -> bool {
        self.tx.send(None).is_ok()
    }

    /// Read the current `IpcInfo`, cloned. Returns `None` if the
    /// loopback hasn't come up yet, is mid-respawn, or will never
    /// come up (builds that skip the loopback transport).
    ///
    /// Clones one `OrchestrationIpcInfo` (two `String` + `PathBuf`
    /// allocations) — avoids pinning the `RwLockReadGuard` borrow of
    /// the watch channel across awaits in adapters. Hot enough to
    /// matter only on session spawn.
    pub fn get(&self) -> Option<OrchestrationIpcInfo> {
        self.rx.borrow().clone()
    }

    /// Lower-level: borrow the current value without cloning. The
    /// returned guard pins the channel so callers should not hold it
    /// across awaits. Used by the rare hot path (e.g. ring-buffer
    /// writes) that can stay synchronous.
    pub fn borrow(&self) -> watch::Ref<'_, Option<OrchestrationIpcInfo>> {
        self.rx.borrow()
    }

    /// Clone the receiver for a caller that wants to `await`
    /// channel updates — useful for supervisors that reconfigure
    /// state when the daemon port changes. Today no adapter does
    /// this; exposed for Phase 6's crash-respawn observer.
    pub fn subscribe(&self) -> watch::Receiver<Option<OrchestrationIpcInfo>> {
        self.rx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(url: &str) -> OrchestrationIpcInfo {
        OrchestrationIpcInfo {
            base_url: url.to_string(),
            executable_path: PathBuf::from("/usr/local/bin/flowstate"),
        }
    }

    #[test]
    fn handle_starts_empty_and_populates() {
        let h = OrchestrationIpcHandle::new();
        assert!(h.get().is_none());
        assert!(h.publish(sample("http://127.0.0.1:12345")));
        assert_eq!(h.get().unwrap().base_url, "http://127.0.0.1:12345");
    }

    #[test]
    fn republish_overwrites_previous_value() {
        // Respawn scenario: daemon dies at port A, new daemon at port B.
        // Adapters that read after republish see the new URL.
        let h = OrchestrationIpcHandle::new();
        h.publish(sample("http://127.0.0.1:1111"));
        h.publish(sample("http://127.0.0.1:2222"));
        assert_eq!(h.get().unwrap().base_url, "http://127.0.0.1:2222");
    }

    #[test]
    fn clear_reverts_to_none() {
        let h = OrchestrationIpcHandle::new();
        h.publish(sample("http://127.0.0.1:1234"));
        assert!(h.get().is_some());
        h.clear();
        assert!(h.get().is_none());
    }

    #[test]
    fn clones_share_the_channel() {
        let a = OrchestrationIpcHandle::new();
        let b = a.clone();
        a.publish(sample("x"));
        assert!(b.get().is_some());
    }

    #[tokio::test]
    async fn subscribe_notifies_on_change() {
        let h = OrchestrationIpcHandle::new();
        let mut rx = h.subscribe();
        h.publish(sample("http://127.0.0.1:55"));
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().as_ref().unwrap().base_url, "http://127.0.0.1:55");
    }
}
