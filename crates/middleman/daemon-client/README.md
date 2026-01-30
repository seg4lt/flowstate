# daemon-client

Thin client every frontend uses to locate — or auto-spawn — a running
`zenui-server` and attach to its HTTP + WS endpoints. Dependency-lean
by design: it does not pull in `runtime-core`, `daemon-core`, or any
provider crate, so a desktop shell that links it doesn't drag in the
full runtime + SQLite + provider stack.

## `connect_or_spawn(&ClientConfig) -> Result<DaemonHandle>`

Race-safe discovery sequence:

1. Read the per-project ready file (same layout as daemon-core's —
   see below).
2. If present, probe `/api/health` with a 500ms timeout. Success →
   return the handle immediately.
3. On missing or stale ready file, acquire an `fs4` exclusive
   advisory lock keyed by a hash of the canonical project root. Retry
   with backoff up to 2 seconds.
4. **Re-read the ready file after acquiring the lock** — another
   client may have won the race while we were blocked.
5. If still absent, invoke `zenui-server start --project-root=<root>`.
   `zenui-server`'s own `start` subcommand handles the detached
   fork-exec and polls its own ready file before returning.
6. Poll the ready file (up to `spawn_timeout`, default 10s) until
   present + healthy; return the `DaemonHandle`.

## `DaemonHandle`

```rust
pub struct DaemonHandle {
    pub http_base: String,  // "http://127.0.0.1:53271"
    pub ws_url:    String,  // "ws://127.0.0.1:53271/ws"
    pub pid:       u32,
}
```

Everything a shell needs to open a webview and connect a WebSocket.

## `ClientConfig`

```rust
pub struct ClientConfig {
    pub project_root:    PathBuf,
    pub server_binary:   Option<PathBuf>,  // tests / dev override
    pub spawn_timeout:   Duration,
    pub health_timeout:  Duration,
}
```

`ClientConfig::for_current_project()` canonicalizes the current working
directory and sets sensible timeout defaults.

## `resolve_server_binary`

Looks in this order:

1. `ClientConfig::server_binary` override.
2. `$ZENUI_SERVER_BIN` environment variable.
3. `current_exe().parent().join("zenui-server")` — the normal case
   when both binaries live in the same `target/debug/` or install
   prefix.

Errors cleanly with a message pointing the user at `ZENUI_SERVER_BIN`
if none of the three succeed.

## Duplicated ready-file format

The read-side `ReadyFile` struct is duplicated from
[`../daemon-core/`](../daemon-core/README.md) (~70 LOC of serde +
path resolution) rather than sharing a crate. The duplication avoids
pulling the runtime stack into `daemon-client`. Both implementations
run under the same Rust toolchain, so `DefaultHasher` produces
matching path digests — the two sides always agree on the location.

## Dependencies

- `fs4` — cross-platform advisory file locking.
- `ureq` — blocking HTTP client for the health probe.
- `anyhow`, `serde`, `serde_json`.

**No** runtime-core, daemon-core, or provider dependencies. Keeping
the shell binary lean is the whole point of this crate.
