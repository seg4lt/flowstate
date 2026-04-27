//! User-defined global MCP server registry.
//!
//! Loads `~/.flowstate/mcp.json` (canonical source of truth) and
//! exposes a validated, deduped snapshot to every provider adapter.
//! Each adapter merges the snapshot into its own provider-native
//! injection path at session spawn time — see the per-adapter
//! plumbing in `provider-claude-cli`, `provider-claude-sdk`,
//! `provider-codex`, `provider-github-copilot{-cli}`, and
//! `provider-opencode`.
//!
//! # Why a single file rather than per-provider config
//!
//! Every provider already speaks the same de-facto MCP schema
//! (`{mcpServers: {...}}` for stdio + `{type, url}` for http/sse),
//! so we don't reinvent anything: we just load the user's list once
//! and feed it into the channel each provider already uses for the
//! flowstate orchestration entry. The user adds an MCP once and it
//! shows up across Claude, Codex, Copilot, OpenCode, etc.
//!
//! # Hot-reload behaviour
//!
//! Load-on-spawn. [`UserMcpRegistry::load`] is called by each
//! adapter inside its session-spawn path; new sessions pick up file
//! changes immediately, running sessions keep the config they were
//! launched with. Live re-injection would require either disruptive
//! provider-process recycling or an MCP-reconnect protocol no
//! provider supports today, so we accept the trade-off.
//!
//! # Reserved key
//!
//! The name `"flowstate"` is reserved for the orchestration server.
//! [`UserMcpRegistry::load`] silently strips any user entry under
//! that key (warns to logs); [`UserMcpRegistry::merge_with_flowstate`]
//! re-inserts the orchestration entry last so a reserved-key
//! collision is impossible end-to-end.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::mcp_config::{McpConfigFile, McpServerConfig, validate_mcp_server_config};

/// Reserved MCP entry name owned by the flowstate orchestration
/// server. User entries under this key are stripped at load time.
pub const RESERVED_FLOWSTATE_KEY: &str = "flowstate";

/// Cheap-to-clone snapshot of the user's MCP server map, returned
/// from [`UserMcpRegistry::load`]. Each adapter takes a snapshot
/// once per spawn and merges entries into its provider-native
/// config shape.
#[derive(Debug, Clone, Default)]
pub struct McpSnapshot {
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpSnapshot {
    /// `true` when the user hasn't defined any MCPs (or all entries
    /// were invalid). Adapters can short-circuit extra work in that
    /// case — though `merge_with_flowstate` is also a no-op when
    /// `servers` is empty.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// Loader for `~/.flowstate/mcp.json`. Cheap to clone (just a path
/// + marker); construct once at daemon setup and pass into every
/// adapter constructor as `Option<UserMcpRegistry>`.
///
/// # Failure model
///
/// Every failure mode (missing file, parse error, invalid entry) is
/// non-fatal — the registry returns an empty snapshot and logs a
/// warning. Rationale: a malformed user-edited config should never
/// brick the daemon. Validation feedback for the UI rides through
/// the dedicated `set_user_mcp_servers` Tauri command, which
/// returns hard errors before writing.
#[derive(Debug, Clone)]
pub struct UserMcpRegistry {
    path: PathBuf,
}

impl UserMcpRegistry {
    /// Construct pointing at `<flowstate_dir>/mcp.json`. The file
    /// need not exist — [`load`] returns an empty snapshot when it
    /// is missing.
    pub fn new(flowstate_dir: &Path) -> Self {
        Self {
            path: flowstate_dir.join("mcp.json"),
        }
    }

    /// The on-disk path this registry reads/writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read, parse, and validate the current contents of the
    /// registry file. Always returns an [`McpSnapshot`] — failures
    /// are logged and yield `Default`.
    ///
    /// Each call hits the filesystem; callers should invoke this
    /// once per session spawn, not in hot paths.
    pub fn load(&self) -> McpSnapshot {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return McpSnapshot::default();
            }
            Err(e) => {
                tracing::warn!(
                    target: "user_mcp",
                    path = %self.path.display(),
                    error = %e,
                    "failed to read mcp.json; treating as empty"
                );
                return McpSnapshot::default();
            }
        };
        let file: McpConfigFile = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    target: "user_mcp",
                    path = %self.path.display(),
                    error = %e,
                    "failed to parse mcp.json; treating as empty"
                );
                return McpSnapshot::default();
            }
        };

        let mut servers = BTreeMap::new();
        for (name, cfg) in file.mcp_servers {
            if name == RESERVED_FLOWSTATE_KEY {
                tracing::warn!(
                    target: "user_mcp",
                    "entry {:?} is reserved for flowstate orchestration; ignoring",
                    name
                );
                continue;
            }
            match validate_mcp_server_config(&name, &cfg) {
                Ok(()) => {
                    servers.insert(name, cfg);
                }
                Err(reason) => {
                    tracing::warn!(target: "user_mcp", "skipping invalid entry: {reason}");
                }
            }
        }
        McpSnapshot { servers }
    }

    /// Merge the flowstate orchestration entry with a user snapshot
    /// into a single [`McpConfigFile`] suitable for writing to disk
    /// (Claude CLI / Copilot CLI) or wrapping into a provider-native
    /// shape. The flowstate entry is inserted **last**, so even if a
    /// user entry slipped through with the reserved key it would be
    /// overwritten — defence in depth.
    pub fn merge_with_flowstate(
        flowstate_entry: McpServerConfig,
        user: &McpSnapshot,
    ) -> McpConfigFile {
        let mut servers = user.servers.clone();
        servers.insert(RESERVED_FLOWSTATE_KEY.to_string(), flowstate_entry);
        McpConfigFile {
            mcp_servers: servers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_config::{flowstate_mcp_entry, McpConfigFile};
    use crate::orchestration_ipc::OrchestrationIpcInfo;

    fn registry_with_file(contents: &str) -> (UserMcpRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), contents).unwrap();
        let registry = UserMcpRegistry::new(dir.path());
        (registry, dir)
    }

    fn sample_ipc() -> OrchestrationIpcInfo {
        OrchestrationIpcInfo {
            base_url: "http://127.0.0.1:54321".to_string(),
            executable_path: PathBuf::from("/usr/local/bin/flowstate"),
        }
    }

    #[test]
    fn load_nonexistent_file_returns_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let registry = UserMcpRegistry::new(dir.path());
        assert!(registry.load().is_empty());
    }

    #[test]
    fn load_valid_file_returns_entries() {
        let body = r#"{
            "mcpServers": {
                "sqlite": { "type": "stdio", "command": "uvx", "args": ["mcp-server-sqlite"] },
                "remote": { "type": "http", "url": "https://example.com/mcp" }
            }
        }"#;
        let (registry, _dir) = registry_with_file(body);
        let snap = registry.load();
        assert_eq!(snap.servers.len(), 2);
        assert!(snap.servers.contains_key("sqlite"));
        assert!(snap.servers.contains_key("remote"));
        assert_eq!(snap.servers["remote"].transport, "http");
    }

    #[test]
    fn load_strips_reserved_flowstate_key() {
        let body = r#"{
            "mcpServers": {
                "flowstate": { "type": "stdio", "command": "/evil/binary" },
                "ok": { "type": "stdio", "command": "/usr/local/bin/srv" }
            }
        }"#;
        let (registry, _dir) = registry_with_file(body);
        let snap = registry.load();
        assert_eq!(snap.servers.len(), 1);
        assert!(!snap.servers.contains_key("flowstate"));
        assert!(snap.servers.contains_key("ok"));
    }

    #[test]
    fn load_skips_invalid_entries() {
        let body = r#"{
            "mcpServers": {
                "bad-stdio": { "type": "stdio" },
                "bad-http": { "type": "http" },
                "bad-transport": { "type": "websocket", "url": "ws://x" },
                "good": { "type": "stdio", "command": "/bin/srv" }
            }
        }"#;
        let (registry, _dir) = registry_with_file(body);
        let snap = registry.load();
        assert_eq!(snap.servers.len(), 1);
        assert!(snap.servers.contains_key("good"));
    }

    #[test]
    fn load_unparseable_json_returns_empty() {
        let (registry, _dir) = registry_with_file("not json at all");
        assert!(registry.load().is_empty());
    }

    #[test]
    fn merge_with_flowstate_inserts_orchestration_entry() {
        let mut user = McpSnapshot::default();
        user.servers.insert(
            "sqlite".to_string(),
            McpServerConfig {
                transport: "stdio".to_string(),
                command: Some("uvx".to_string()),
                args: vec!["mcp-server-sqlite".to_string()],
                env: None,
                url: None,
            },
        );
        let merged = UserMcpRegistry::merge_with_flowstate(
            flowstate_mcp_entry(&sample_ipc(), "s1"),
            &user,
        );
        assert!(merged.mcp_servers.contains_key("flowstate"));
        assert!(merged.mcp_servers.contains_key("sqlite"));
        assert_eq!(merged.mcp_servers.len(), 2);
    }

    #[test]
    fn merge_with_flowstate_user_cannot_shadow_reserved_key() {
        // Direct call into the merge helper with a hand-crafted
        // snapshot that contains "flowstate" — bypasses load() which
        // would have stripped it. The merge must still produce the
        // real orchestration entry.
        let mut user = McpSnapshot::default();
        user.servers.insert(
            "flowstate".to_string(),
            McpServerConfig {
                transport: "stdio".to_string(),
                command: Some("/evil/binary".to_string()),
                args: vec![],
                env: None,
                url: None,
            },
        );
        let real = flowstate_mcp_entry(&sample_ipc(), "s1");
        let merged = UserMcpRegistry::merge_with_flowstate(real, &user);
        let entry = &merged.mcp_servers["flowstate"];
        assert_eq!(
            entry.command.as_deref(),
            Some("/usr/local/bin/flowstate"),
            "real flowstate entry must override user-supplied one"
        );
    }

    #[test]
    fn merge_serializes_to_standard_envelope() {
        let user = McpSnapshot::default();
        let merged = UserMcpRegistry::merge_with_flowstate(
            flowstate_mcp_entry(&sample_ipc(), "s1"),
            &user,
        );
        // Round-trip: the merged map must serialize/deserialize as a
        // standard McpConfigFile envelope every provider understands.
        let json = serde_json::to_string(&merged).unwrap();
        assert!(json.contains("\"mcpServers\""));
        let read: McpConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(read.mcp_servers.len(), 1);
    }
}
