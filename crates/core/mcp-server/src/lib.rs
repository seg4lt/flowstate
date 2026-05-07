//! Standalone MCP stdio server that proxies the flowstate orchestration
//! tool surface to any provider agent that can mount a local MCP
//! subprocess.
//!
//! # Why this exists
//!
//! The Claude Agent SDK has first-class support for in-process MCP
//! servers (see `crates/core/provider-claude-sdk/bridge/src/index.ts`
//! for how we register flowstate's tools inside that bridge). None of
//! the other providers do:
//!
//! - `@github/copilot-sdk` exports a `defineTool()` type but never
//!   calls it — tools must come from `SessionConfig.mcpServers` as
//!   stdio or HTTP subprocesses.
//! - opencode registers MCP servers through `opencode mcp add` /
//!   `opencode.json`; per-session in-process tools aren't a thing.
//! - Codex CLI, Claude CLI, and Copilot CLI each consume a local
//!   `.mcp.json` or equivalent config file.
//!
//! Stdio subprocess is the **only transport every provider supports**.
//! So this binary:
//!
//! 1. Speaks MCP stdio (JSON-RPC 2.0, one message per line).
//! 2. Fetches the tool catalog from the running flowstate daemon at
//!    startup — `GET $FLOWSTATE_HTTP_BASE/api/orchestration/catalog`.
//!    The daemon's catalog is derived from
//!    `zenui_provider_api::capability_tools_wire()`, which is the
//!    single source of truth for the orchestration surface.
//! 3. On every `tools/call` the MCP client issues, POSTs the call to
//!    `$FLOWSTATE_HTTP_BASE/api/orchestration/dispatch` with the
//!    originating session id (`$FLOWSTATE_SESSION_ID`) and relays the
//!    runtime's result back to the MCP client verbatim.
//!
//! Intentionally no in-process state. A crashed MCP server is
//! restartable transparently — all orchestration state lives in the
//! daemon.
//!
//! # Protocol
//!
//! Implements the subset of MCP every stdio-capable agent needs:
//! `initialize`, `tools/list`, `tools/call`. Notifications are
//! accepted and ignored. Anything else returns a JSON-RPC method-not-
//! found error so clients get a clear diagnostic instead of a hang.
//!
//! # Configuration
//!
//! Env vars are the default path (Copilot SDK / Claude CLI supply them
//! naturally through `SessionConfig.mcpServers.env` / `.mcp.json`). CLI
//! flags override them and make writing opencode-style static configs
//! (`opencode.json`) practical — opencode's `McpLocalConfig.environment`
//! is a static dict, so passing the session id via `--session-id` in
//! the `command` array is the only per-session-clean path.
//!
//! - `--http-base <URL>` / `FLOWSTATE_HTTP_BASE` (required) — base URL
//!   of the running flowstate daemon's HTTP transport, e.g.
//!   `http://127.0.0.1:4873`.
//! - `--session-id <ID>` / `FLOWSTATE_SESSION_ID` (required) — the
//!   session id the dispatched `RuntimeCall`s will carry as
//!   `origin.session_id`. Provider adapters set this when they spawn
//!   the MCP server subprocess (one subprocess per session).
//! - `--timeout-secs <N>` / `FLOWSTATE_HTTP_TIMEOUT_SECS` (optional,
//!   default 1800) — per-dispatch HTTP timeout. Matches
//!   `DEFAULT_AWAIT_TIMEOUT_SECS` in `runtime-core/src/orchestration.rs`.

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

/// MCP protocol version this server speaks. Clients pin on the
/// handshake and decide whether they can talk to us.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30 * 60;

#[derive(Debug, Clone)]
struct Config {
    http_base: String,
    session_id: String,
    http_timeout: Duration,
}

impl Config {
    /// Parse config from process argv + env. CLI flags (`--http-base`,
    /// `--session-id`, `--timeout-secs`) win over their env var
    /// equivalents — this lets opencode-style static configs bake the
    /// session id into the `command` array verbatim, since opencode's
    /// `McpLocalConfig.environment` is a static dict shared across
    /// every session that uses the config.
    ///
    /// Invoked as `flowstate mcp-server …`, so argv looks like
    /// `[<exe>, "mcp-server", "--http-base", URL, "--session-id", SID]`.
    /// We skip both argv[0] (the binary path) AND argv[1] (the
    /// subcommand name that main.rs already dispatched on). Without
    /// this, the parser saw "mcp-server" as the first flag and
    /// errored with `unknown flag: mcp-server`, which manifested as
    /// `MCP error -32000: Connection closed` on the opencode side —
    /// the subprocess crashed before emitting its ready handshake.
    fn resolve() -> Result<Self> {
        let mut args = env::args().skip(2);
        let mut http_base = env::var("FLOWSTATE_HTTP_BASE").ok();
        let mut session_id = env::var("FLOWSTATE_SESSION_ID").ok();
        let mut http_timeout_secs = env::var("FLOWSTATE_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--http-base" => {
                    http_base = Some(args.next().context("--http-base requires a URL argument")?);
                }
                "--session-id" => {
                    session_id = Some(
                        args.next()
                            .context("--session-id requires an id argument")?,
                    );
                }
                "--timeout-secs" => {
                    http_timeout_secs = args
                        .next()
                        .context("--timeout-secs requires a number argument")?
                        .parse::<u64>()
                        .context("--timeout-secs must be a positive integer")?;
                }
                "--help" | "-h" => {
                    eprintln!(
                        "flowstate-mcp-server — MCP stdio proxy for the flowstate daemon\n\
                         \n\
                         Usage: flowstate-mcp-server --http-base URL --session-id ID [--timeout-secs N]\n\
                         \n\
                         Env fallbacks: FLOWSTATE_HTTP_BASE, FLOWSTATE_SESSION_ID, FLOWSTATE_HTTP_TIMEOUT_SECS\n\
                         \n\
                         No authentication on the target HTTP — the daemon binds 127.0.0.1 only,\n\
                         which on a single-user desktop is the intended boundary.\n\
                         \n\
                         Reads JSON-RPC on stdin, writes on stdout. Logs on stderr."
                    );
                    std::process::exit(0);
                }
                other => bail!("unknown flag: {other}"),
            }
        }
        let http_base = http_base
            .context("http base URL required (--http-base URL or FLOWSTATE_HTTP_BASE env var)")?;
        let session_id = session_id
            .context("session id required (--session-id ID or FLOWSTATE_SESSION_ID env var)")?;
        Ok(Self {
            http_base,
            session_id,
            http_timeout: Duration::from_secs(http_timeout_secs),
        })
    }
}

/// Tool catalog entry as returned by `GET /api/orchestration/catalog`.
/// Mirrors `zenui_provider_api::ToolCatalogEntry` but deserialized
/// here so the binary has no compile-time coupling to the daemon
/// crate — only the JSON wire shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ToolEntry {
    name: String,
    description: String,
    input_schema: Value,
}

async fn fetch_catalog(client: &Client, cfg: &Config) -> Result<Vec<ToolEntry>> {
    let url = format!("{}/api/orchestration/catalog", cfg.http_base);
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("catalog fetch returned {status}: {body}");
    }
    let entries: Vec<ToolEntry> = response
        .json()
        .await
        .context("catalog response was not valid JSON")?;
    info!(count = entries.len(), "fetched flowstate tool catalog");
    Ok(entries)
}

/// POST /api/orchestration/dispatch and return the raw JSON the
/// daemon produced. The daemon's response envelope is
/// `{ "result": <RuntimeCallResult> }` on success or
/// `{ "error": <RuntimeCallError> }` on dispatcher error — both are
/// 200 responses; we distinguish at the caller.
async fn dispatch_tool(
    client: &Client,
    cfg: &Config,
    tool_name: &str,
    args: &Value,
) -> Result<DispatchResponse> {
    let url = format!("{}/api/orchestration/dispatch", cfg.http_base);
    // The daemon expects `{ origin_session_id, origin_turn_id?,
    // kind, ..<RuntimeCall fields> }`. Keys:
    //
    // - `origin_session_id` — WHO is calling (us). Derived from
    //   `cfg.session_id`, which provider adapters bake into the MCP
    //   command array or env at spawn time. Intentionally named
    //   `origin_*` (not plain `session_id`) because several
    //   `RuntimeCall` variants have their OWN `session_id` field
    //   naming the TARGET peer, and `#[serde(flatten)]` on the outer
    //   struct would otherwise collide and produce a cryptic
    //   "missing field session_id" error on every poll / send call.
    //
    // - `kind` — tool name; `RuntimeCall` is `#[serde(tag = "kind")]`.
    //
    // - The rest of the agent's args (`session_id`, `message`,
    //   `initial_message`, `since_turn_id`, …) flatten into the
    //   inner variant untouched.
    let body = match args {
        Value::Object(map) => {
            let mut merged = map.clone();
            merged.insert(
                "origin_session_id".to_string(),
                Value::String(cfg.session_id.clone()),
            );
            merged.insert("kind".to_string(), Value::String(tool_name.to_string()));
            Value::Object(merged)
        }
        _ => bail!("tool arguments must be a JSON object, got {args:?}"),
    };
    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to POST {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("dispatch returned {status}: {body}");
    }
    response
        .json::<DispatchResponse>()
        .await
        .context("dispatch response was not a valid DispatchResponse")
}

#[derive(Debug, Deserialize)]
struct DispatchResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP JSON-RPC 2.0 framing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// Standard JSON-RPC error codes; see
// https://www.jsonrpc.org/specification#error_object
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

fn ok_response(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Value, code: i32, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data: None,
        }),
    }
}

/// Convert a flowstate dispatch response into the MCP `tools/call`
/// result envelope. MCP expects `{ content: [{ type: "text", text }] }`
/// on success and `{ content: [...], isError: true }` on tool-level
/// failure. We stringify the runtime's JSON payload as text so the
/// agent sees structured content it can parse. Mirrors the Claude
/// SDK bridge's `dispatchRuntimeCall` return shape for consistency.
fn render_tool_result(response: DispatchResponse) -> Value {
    if let Some(error) = response.error {
        return json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&error).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
            }],
            "isError": true,
        });
    }
    let payload = response.result.unwrap_or(Value::Null);
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string(&payload).unwrap_or_else(|e| format!("\"{e}\"")),
        }],
    })
}

/// Translate a ToolCatalogEntry to an MCP `tools/list` entry. MCP
/// wants `inputSchema` (camelCase) and we return `input_schema` on
/// the flowstate wire — the only reshape step in the whole loop.
fn render_tool_definition(entry: &ToolEntry) -> Value {
    json!({
        "name": entry.name,
        "description": entry.description,
        "inputSchema": entry.input_schema,
    })
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

struct Server {
    cfg: Config,
    http: Client,
    catalog: Vec<ToolEntry>,
}

impl Server {
    async fn handle(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        // Notifications (no `id`) must not produce a response per
        // JSON-RPC spec. We support `notifications/initialized` which
        // MCP clients send after a successful handshake.
        let id = match req.id {
            Some(id) => id,
            None => {
                debug!(method = %req.method, "notification (ignored)");
                return None;
            }
        };
        if req.jsonrpc != "2.0" {
            warn!(%req.jsonrpc, "non-2.0 JSON-RPC request; continuing anyway");
        }

        let response = match req.method.as_str() {
            "initialize" => self.handle_initialize(id).await,
            "tools/list" => self.handle_tools_list(id).await,
            "tools/call" => self.handle_tools_call(id, req.params).await,
            "ping" => ok_response(id, json!({})),
            other => error_response(
                id,
                METHOD_NOT_FOUND,
                format!("method `{other}` not implemented by flowstate-mcp-server"),
            ),
        };
        Some(response)
    }

    async fn handle_initialize(&self, id: Value) -> JsonRpcResponse {
        ok_response(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "flowstate-mcp-server",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
    }

    async fn handle_tools_list(&self, id: Value) -> JsonRpcResponse {
        let tools: Vec<Value> = self.catalog.iter().map(render_tool_definition).collect();
        ok_response(id, json!({ "tools": tools }))
    }

    async fn handle_tools_call(&self, id: Value, params: Value) -> JsonRpcResponse {
        let name = match params.get("name").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None => {
                return error_response(
                    id,
                    INVALID_PARAMS,
                    "tools/call requires `name`".to_string(),
                );
            }
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match dispatch_tool(&self.http, &self.cfg, &name, &args).await {
            Ok(response) => ok_response(id, render_tool_result(response)),
            Err(err) => {
                error!(%err, tool = %name, "dispatch failed");
                error_response(id, INTERNAL_ERROR, format!("dispatch failed: {err}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// stdio event loop
// ---------------------------------------------------------------------------

async fn write_response(stdout: &mut tokio::io::Stdout, response: JsonRpcResponse) -> Result<()> {
    let line = serde_json::to_string(&response).context("serialize response")?;
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

/// Async event loop: read stdin line-by-line, dispatch each JSON-RPC
/// request, write responses to stdout. Called by [`run_blocking`]
/// after a dedicated tokio runtime has been built, and reusable from
/// any other tokio context (e.g. integration tests).
/// Spawn a background task that polls every 2 s and forces the
/// process to exit if its grand-parent flowstate has died. Two
/// independent signals — either one triggers shutdown:
///
/// 1. **Reparenting to init** (`getppid() == 1`): when a direct parent
///    dies, Unix kernels reparent the child to PID 1. Our direct
///    parent is the *agent* (opencode, codex, …), not flowstate,
///    but in practice when flowstate is SIGKILL'd the agent subprocess
///    tree also dies (opencode's `kill_on_drop` usually fires, or the
///    agent exits when its MCP stdio connection goes silent), so
///    reparenting is a strong signal in the SIGKILL path too.
/// 2. **Direct pid probe** (`kill(FLOWSTATE_PID, 0)`): returns 0 iff
///    the pid still exists. Catches the case where the agent survives
///    flowstate (e.g. opencode serve's detached reader keeps draining
///    stdin).
///
/// Signal 1 false-positives if the agent genuinely outlives flowstate
/// by design — but no agent we wire today does that, and in any case
/// signal 2 is the authoritative check. Signal 1 is kept as a cheap
/// first filter so we don't syscall `kill` every tick on the common
/// healthy case.
///
/// 2-second latency is fine — this is a cleanup mechanism, not a
/// hot-path. `std::process::exit(0)` (not `1`) because a stale parent
/// isn't an error; the MCP client will see its stdin/stdout close and
/// treat it as a clean subprocess shutdown.
#[cfg(unix)]
fn spawn_parent_watchdog(flowstate_pid: u32) {
    tokio::spawn(async move {
        // Guard against pathological FLOWSTATE_PID=1 (would mean
        // flowstate IS init — impossible in real deploys; in test
        // harnesses the value would be wrong anyway). Short-circuit
        // rather than tripping false positives.
        if flowstate_pid <= 1 {
            warn!(
                pid = flowstate_pid,
                "FLOWSTATE_PID unusable; watchdog disabled"
            );
            return;
        }
        let flowstate_pid = flowstate_pid as libc::pid_t;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let reparented = unsafe { libc::getppid() } == 1;
            // `kill(pid, 0)` returns 0 if the process exists and the
            // caller has permission to signal it, else -1 with errno
            // ESRCH (no such process). Any non-zero return on a pid
            // we legitimately stamped means the parent is gone.
            let flowstate_dead = unsafe { libc::kill(flowstate_pid, 0) } != 0;
            if reparented || flowstate_dead {
                info!(
                    reparented,
                    flowstate_dead,
                    flowstate_pid = flowstate_pid as i64,
                    "flowstate gone; MCP subprocess self-exiting"
                );
                // exit(0) — a missing parent is not our error.
                // Using libc::_exit would skip tokio's drain; the
                // normal std::process::exit is fine here because the
                // stdio proxy has no persistent state to flush.
                std::process::exit(0);
            }
        }
    });
}

/// Non-Unix fallback: no cheap equivalent of `getppid()` /
/// `kill(pid, 0)`. Could use `sysinfo::System::process()` but that
/// would pull in a ~MB dep for Windows only where flowstate doesn't
/// ship a tauri-dev SIGKILL path to worry about. Keep the stub so
/// callers compile cross-platform and revisit when Windows ships.
#[cfg(not(unix))]
fn spawn_parent_watchdog(_flowstate_pid: u32) {}

pub async fn run() -> Result<()> {
    let cfg = Config::resolve()?;

    // Parent-liveness watchdog: if flowstate dies (SIGKILL during
    // `tauri dev` reload, crash, Activity Monitor force-quit), this
    // subprocess would otherwise reparent to PID 1 and survive,
    // pointing at a now-dead loopback port. Detect via two cheap
    // syscalls polled every 2 s:
    //   - `getppid() == 1` (reparented to init → orphaned)
    //   - `kill(FLOWSTATE_PID, 0)` fails (original parent pid gone)
    // Either condition ⇒ self-exit. See the FLOWSTATE_PID notes in
    // `crates/core/provider-api/src/mcp_config.rs`.
    if let Some(pid_str) = env::var("FLOWSTATE_PID").ok() {
        if let Ok(pid) = pid_str.parse::<u32>() {
            spawn_parent_watchdog(pid);
        } else {
            warn!(%pid_str, "FLOWSTATE_PID not a positive integer; watchdog disabled");
        }
    }

    let http = Client::builder()
        .timeout(cfg.http_timeout)
        .build()
        .context("build HTTP client")?;

    let catalog = fetch_catalog(&http, &cfg).await?;
    info!(
        session = %cfg.session_id,
        http_base = %cfg.http_base,
        tools = catalog.len(),
        "flowstate-mcp-server ready"
    );

    let server = Server { cfg, http, catalog };

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await.context("read from stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(err) => {
                // Malformed input is non-fatal — log, keep serving so
                // a bad line doesn't kill the session.
                warn!(%err, %line, "skipping malformed JSON-RPC request");
                continue;
            }
        };
        if let Some(response) = server.handle(req).await {
            if let Err(err) = write_response(&mut stdout, response).await {
                error!(%err, "failed to write response; exiting");
                return Err(anyhow!("stdout write failed: {err}"));
            }
        }
    }

    info!("stdin closed; exiting");
    Ok(())
}

/// Blocking entry point used by the `flowstate` binary's argv
/// dispatcher in `apps/flowstate/src-tauri/src/main.rs`. Owns its own
/// multi-threaded tokio runtime — callers that already have a runtime
/// (tests, in-process embedders) should call [`run`] directly.
///
/// Initialises a stderr-writing `tracing_subscriber` so logs don't
/// collide with the stdout JSON-RPC stream the MCP client is reading.
/// Default level is `info`; override with `RUST_LOG=…`.
pub fn run_blocking() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for flowstate mcp-server")?;
    runtime.block_on(run())
}
