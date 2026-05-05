//! Helper for rendering the MCP-server config JSON that provider
//! adapters drop on disk for file-based CLI agents (Codex).
//!
//! # Why a shared helper
//!
//! `provider-codex` needs to register the `flowstate mcp-server`
//! subprocess with its underlying CLI so the agent can call the
//! cross-provider orchestration tools. The shape of the config follows
//! the `{mcpServers: {name: {command, args, env}}}` JSON schema that
//! originated with the MCP reference implementation. Codex uses TOML
//! with the same shape; we render JSON-centered here and let the Codex
//! adapter pass fields via its `-c` CLI flag instead of writing a
//! file.
//!
//! Keep the output format locked here so a drift fix is one edit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::orchestration_ipc::OrchestrationIpcInfo;

/// One entry in the `mcpServers` map. Mirrors the shape the MCP
/// reference (`@modelcontextprotocol/server-*`) established and that
/// every stdio-capable client we care about parses verbatim.
///
/// Supports three transports — `"stdio"` (subprocess; `command`
/// required), `"http"` and `"sse"` (remote; `url` required). The
/// flowstate orchestration entry is always stdio; user-defined MCPs
/// loaded from `~/.flowstate/mcp.json` may use any of the three.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Transport kind. One of `"stdio"`, `"http"`, `"sse"`. Defaults
    /// to `"stdio"` for files written before the multi-transport
    /// schema landed (Claude CLI / Codex `.mcp.json` historically
    /// omitted the field when `command` was present). Owned `String`
    /// so the struct survives `serde_json::from_str`'s lifetime rules
    /// in tests and callers that read an existing `.mcp.json` back.
    #[serde(rename = "type", default = "default_transport")]
    pub transport: String,
    /// Required for stdio; `None` for http/sse.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    /// argv for stdio. `default` lets old/minimal entries omit the
    /// field; `skip_serializing_if` keeps the rendered file tidy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for stdio processes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Base URL for http/sse transport (e.g. `https://mcp.example.com/sse`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url: Option<String>,
}

fn default_transport() -> String {
    "stdio".to_string()
}

/// Validate a single MCP server entry against the transport rules.
/// Returns `Err` with a human-readable reason on invalid shapes
/// (unknown transport, stdio without command, http/sse without url).
/// Used by [`crate::user_mcp::UserMcpRegistry`] to silently drop bad
/// entries from `~/.flowstate/mcp.json` rather than fail-fast.
pub fn validate_mcp_server_config(name: &str, cfg: &McpServerConfig) -> Result<(), String> {
    match cfg.transport.as_str() {
        "stdio" => {
            if cfg.command.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "MCP server {name:?}: stdio transport requires non-empty `command`"
                ));
            }
        }
        "http" | "sse" => {
            if cfg.url.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "MCP server {name:?}: {transport} transport requires non-empty `url`",
                    transport = cfg.transport
                ));
            }
        }
        other => {
            return Err(format!(
                "MCP server {name:?}: unknown transport {other:?} (expected stdio|http|sse)"
            ));
        }
    }
    Ok(())
}

/// Full `{mcpServers: {…}}` envelope — the shape of a `.mcp.json`
/// file on disk. Serialize this to `serde_json` and write to the
/// agent's cwd (or the session-scoped dir + `--mcp-config PATH`
/// flag) before spawning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: std::collections::BTreeMap<String, McpServerConfig>,
}

/// Build the `flowstate`-scoped MCP server entry from a live
/// [`OrchestrationIpcInfo`]. Call sites pass it through
/// [`write_mcp_config_file`] or embed into their native config shape
/// (Codex `-c` flags, opencode.json, Copilot `SessionConfig`).
pub fn flowstate_mcp_entry(info: &OrchestrationIpcInfo, session_id: &str) -> McpServerConfig {
    let mut env = std::collections::BTreeMap::new();
    // Environment is redundant with the argv flags the subprocess
    // parses first — we plant both so callers whose config store
    // only lets them set one (env vs args) still work. Loopback
    // HTTP is unauthenticated (see the note in
    // `orchestration_ipc.rs`), so no bearer token rides here.
    env.insert("FLOWSTATE_SESSION_ID".to_string(), session_id.to_string());
    env.insert("FLOWSTATE_HTTP_BASE".to_string(), info.base_url.clone());
    // `FLOWSTATE_PID` lets the stdio proxy subprocess watchdog its
    // grand-parent (flowstate) liveness. When flowstate dies the
    // proxy's `getppid()` flips to 1 (orphaned) and / or `kill(PID, 0)`
    // starts failing — either signal causes the proxy to self-exit
    // within ~2 s, so no zombie mcp-server processes survive the app.
    // See `crates/core/mcp-server/src/lib.rs::spawn_parent_watchdog`.
    env.insert("FLOWSTATE_PID".to_string(), std::process::id().to_string());

    McpServerConfig {
        transport: "stdio".to_string(),
        command: Some(info.executable_path.to_string_lossy().into_owned()),
        args: vec![
            "mcp-server".to_string(),
            "--http-base".to_string(),
            info.base_url.clone(),
            "--session-id".to_string(),
            session_id.to_string(),
        ],
        env: Some(env),
        url: None,
    }
}

/// Convenience: wrap a single flowstate entry in the full
/// `{mcpServers: {flowstate: …}}` envelope.
pub fn flowstate_mcp_config_file(info: &OrchestrationIpcInfo, session_id: &str) -> McpConfigFile {
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "flowstate".to_string(),
        flowstate_mcp_entry(info, session_id),
    );
    McpConfigFile {
        mcp_servers: servers,
    }
}

/// Atomically write an [`McpConfigFile`] to `path`. Returns the path
/// on success so callers can chain into a `--mcp-config` CLI flag.
/// Overwrites any previous content (sessions may rebuild their
/// config on every spawn — token refreshes, base URL changes).
pub fn write_mcp_config_file(path: &Path, config: &McpConfigFile) -> std::io::Result<PathBuf> {
    let body = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("mcp.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> OrchestrationIpcInfo {
        OrchestrationIpcInfo {
            base_url: "http://127.0.0.1:54321".to_string(),
            executable_path: PathBuf::from("/Applications/flowstate.app/Contents/MacOS/flowstate"),
        }
    }

    #[test]
    fn entry_embeds_session_id_in_argv_and_env() {
        let e = flowstate_mcp_entry(&sample_info(), "sess-xyz");
        assert_eq!(e.transport, "stdio");
        assert!(e.args.iter().any(|a| a == "--session-id"));
        assert!(e.args.iter().any(|a| a == "sess-xyz"));
        assert!(e.args.iter().any(|a| a == "--http-base"));
        let env = e.env.unwrap();
        assert_eq!(env.get("FLOWSTATE_SESSION_ID").unwrap(), "sess-xyz");
        assert_eq!(
            env.get("FLOWSTATE_HTTP_BASE").unwrap(),
            "http://127.0.0.1:54321"
        );
        // No auth token plumbed — loopback bind is the only boundary.
        assert!(env.get("FLOWSTATE_AUTH_TOKEN").is_none());
        // Parent pid is stamped so the subprocess can self-terminate
        // when flowstate dies (see `mcp-server`'s parent watchdog).
        let pid = env
            .get("FLOWSTATE_PID")
            .expect("FLOWSTATE_PID must be present")
            .parse::<u32>()
            .expect("FLOWSTATE_PID must be numeric");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn config_file_wraps_entry_under_flowstate_key() {
        let cf = flowstate_mcp_config_file(&sample_info(), "sess-1");
        assert!(cf.mcp_servers.contains_key("flowstate"));
        let json = serde_json::to_string(&cf).unwrap();
        assert!(json.contains("\"mcpServers\""));
        assert!(json.contains("\"flowstate\""));
    }

    #[test]
    fn write_mcp_config_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flowstate.mcp.json");
        let cf = flowstate_mcp_config_file(&sample_info(), "s1");
        write_mcp_config_file(&path, &cf).unwrap();
        let read: McpConfigFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read.mcp_servers.len(), 1);
    }

    #[test]
    fn schema_round_trips_http_entry() {
        let json = r#"{
            "type": "http",
            "url": "https://mcp.example.com/v1"
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport, "http");
        assert_eq!(cfg.url.as_deref(), Some("https://mcp.example.com/v1"));
        assert!(cfg.command.is_none());
        let back = serde_json::to_string(&cfg).unwrap();
        // command/env/args omitted (skip_serializing_if).
        assert!(!back.contains("\"command\""));
        assert!(!back.contains("\"args\""));
    }

    #[test]
    fn schema_round_trips_sse_entry() {
        let json = r#"{
            "type": "sse",
            "url": "https://mcp.example.com/sse"
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport, "sse");
        assert_eq!(cfg.url.as_deref(), Some("https://mcp.example.com/sse"));
    }

    #[test]
    fn backward_compat_stdio_missing_transport_field() {
        // Legacy `.mcp.json` files written before the multi-transport
        // schema may omit `type` entirely. Default kicks in.
        let json = r#"{
            "command": "uvx",
            "args": ["mcp-server-sqlite", "--db-path", "/tmp/x.db"]
        }"#;
        let cfg: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.transport, "stdio");
        assert_eq!(cfg.command.as_deref(), Some("uvx"));
        assert_eq!(cfg.args.len(), 3);
        assert!(cfg.url.is_none());
    }

    #[test]
    fn validate_rejects_stdio_without_command() {
        let cfg = McpServerConfig {
            transport: "stdio".to_string(),
            command: None,
            args: vec![],
            env: None,
            url: None,
        };
        assert!(validate_mcp_server_config("bad", &cfg).is_err());
    }

    #[test]
    fn validate_rejects_http_without_url() {
        let cfg = McpServerConfig {
            transport: "http".to_string(),
            command: None,
            args: vec![],
            env: None,
            url: None,
        };
        assert!(validate_mcp_server_config("bad", &cfg).is_err());
    }

    #[test]
    fn validate_rejects_unknown_transport() {
        let cfg = McpServerConfig {
            transport: "websocket".to_string(),
            command: None,
            args: vec![],
            env: None,
            url: Some("ws://x".to_string()),
        };
        assert!(validate_mcp_server_config("bad", &cfg).is_err());
    }

    #[test]
    fn validate_accepts_well_formed_entries() {
        let stdio = McpServerConfig {
            transport: "stdio".to_string(),
            command: Some("/usr/local/bin/srv".to_string()),
            args: vec!["--port".to_string(), "9000".to_string()],
            env: None,
            url: None,
        };
        assert!(validate_mcp_server_config("ok-stdio", &stdio).is_ok());

        let http = McpServerConfig {
            transport: "http".to_string(),
            command: None,
            args: vec![],
            env: None,
            url: Some("https://example.com".to_string()),
        };
        assert!(validate_mcp_server_config("ok-http", &http).is_ok());
    }
}
