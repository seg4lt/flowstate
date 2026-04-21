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
//! 2. The bearer token for `Authorization: Bearer …`.
//! 3. The absolute path to the flowstate binary itself (provider
//!    config files that accept a static `command` array — e.g.
//!    `opencode.json`, `.mcp.json` — cannot use `$PATH` lookups).
//!
//! All three are known only *after* the runtime has bound its
//! loopback listener (the port is ephemeral) but adapters are
//! constructed *before* the runtime finishes bootstrapping. This
//! handle bridges that temporal gap: the app creates an empty
//! [`OrchestrationIpcHandle`] up front, clones it into every adapter
//! that needs it, then calls [`OrchestrationIpcHandle::set`] once the
//! loopback transport is live. Adapters read back via
//! [`OrchestrationIpcHandle::get`] at session-spawn time.
//!
//! # Populated-once contract
//!
//! The underlying `OnceCell` can only be filled once per app launch.
//! Callers that observe `None` while spawning a session should treat
//! it as "orchestration not available for this session" rather than
//! an error — dev builds that don't mount the loopback transport are
//! a legitimate use case (orchestration tools just aren't exposed to
//! non-Claude-SDK agents until the cell is populated). The Claude
//! SDK bridge is unaffected: it registers orchestration tools
//! in-process via `createSdkMcpServer` and doesn't consult this
//! handle at all.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::OnceCell;

/// Concrete values a provider adapter needs to spawn an MCP
/// subprocess: the loopback URL to call, the auth token, and the
/// absolute path to the `flowstate` binary (so MCP config files can
/// embed the full command-line). `executable_path` should resolve to
/// the same binary that's serving the HTTP transport — both the
/// Tauri app and any future standalone daemon populate this from
/// `std::env::current_exe()`.
#[derive(Debug, Clone)]
pub struct OrchestrationIpcInfo {
    pub base_url: String,
    pub auth_token: String,
    pub executable_path: PathBuf,
}

/// Cheap-to-clone handle over a once-populated
/// [`OrchestrationIpcInfo`]. Adapters keep a clone; the embedder
/// populates it after the loopback transport binds. Failures to
/// populate are logged at the embedder, not surfaced here — adapters
/// either see `Some(info)` and wire orchestration or see `None` and
/// skip it.
#[derive(Clone, Default, Debug)]
pub struct OrchestrationIpcHandle(Arc<OnceCell<OrchestrationIpcInfo>>);

impl OrchestrationIpcHandle {
    pub fn new() -> Self {
        Self(Arc::new(OnceCell::new()))
    }

    /// Populate the cell. Returns `Err(info)` if the cell already
    /// holds a value — callers should treat this as a logic error
    /// (two loopback transports would break the one-to-one session-
    /// id-to-origin contract the orchestration dispatcher relies on).
    pub fn set(&self, info: OrchestrationIpcInfo) -> Result<(), OrchestrationIpcInfo> {
        // tokio's `OnceCell::set` returns `SetError<T>` whose
        // `AlreadyInitializedError(T)` variant wraps the rejected
        // value. Map through to `Err(info)` so callers don't have
        // to import a tokio-specific error type.
        self.0.set(info).map_err(|e| match e {
            tokio::sync::SetError::AlreadyInitializedError(v) => v,
            tokio::sync::SetError::InitializingError(v) => v,
        })
    }

    /// Read the populated info, or `None` if the loopback transport
    /// hasn't come up yet (or ever, in builds that don't mount it).
    /// Hot path: called from session-spawn on every non-Claude-SDK
    /// provider — must stay allocation-free.
    pub fn get(&self) -> Option<&OrchestrationIpcInfo> {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_starts_empty_and_populates_once() {
        let h = OrchestrationIpcHandle::new();
        assert!(h.get().is_none());
        let info = OrchestrationIpcInfo {
            base_url: "http://127.0.0.1:12345".to_string(),
            auth_token: "tok".to_string(),
            executable_path: PathBuf::from("/usr/local/bin/flowstate"),
        };
        assert!(h.set(info.clone()).is_ok());
        assert_eq!(h.get().unwrap().base_url, "http://127.0.0.1:12345");
        // Second set is rejected.
        assert!(h.set(info).is_err());
    }

    #[test]
    fn clones_share_the_cell() {
        let a = OrchestrationIpcHandle::new();
        let b = a.clone();
        a.set(OrchestrationIpcInfo {
            base_url: "x".into(),
            auth_token: "y".into(),
            executable_path: PathBuf::from("/z"),
        })
        .unwrap();
        assert!(b.get().is_some());
    }
}
