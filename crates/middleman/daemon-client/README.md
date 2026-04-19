# daemon-client

> **Status:** dormant in this repo. Flowstate embeds the runtime
> in-process via `transport-tauri` and does not use a separate daemon
> binary, so nothing here links `daemon-client` today. The crate is
> kept for future out-of-process deployments (e.g. a Unix-socket
> daemon, a CLI shell) and for reference. If you're only working on
> flowstate, you can ignore this crate.

Thin client for locating — or auto-spawning — a running daemon and
attaching to whichever transport it speaks. Dependency-lean by design:
depends only on `fs4`, `ureq`, `serde`, and `anyhow`. **Does not**
link `runtime-core`, `daemon-core`, any transport crate, or any
provider — a shell that uses `daemon-client` doesn't drag in SQLite,
axum, or the full runtime stack.

## `connect_or_spawn(&ClientConfig) -> Result<DaemonHandle>`

Race-safe discovery sequence:

1. Read the per-project ready file.
2. Find a transport matching `config.preferred_transport`. If present,
   probe its liveness (`/api/health` for HTTP; stub `true` for others).
   On success → return the handle.
3. On missing / stale ready file, acquire an `fs4` exclusive advisory
   lock keyed by a hash of the canonical project root. Retries with
   backoff up to 2 seconds.
4. Re-read the ready file under the lock — another client may have
   won the race.
5. If still absent, invoke the configured daemon binary with
   `start --project-root=<root>`. The daemon's own `start` subcommand
   is expected to handle the detached fork-exec and poll its own
   ready file before returning.
6. Poll the ready file (up to `spawn_timeout`, default 10s) until a
   matching transport is present and healthy; return the `DaemonHandle`.

If the running daemon exists but **offers no transport the caller
speaks**, `connect_or_spawn` errors with a clear message naming the
available kinds and suggesting a stop + restart with the desired
transport list. It does NOT attempt to spawn a second daemon on top
of the existing one.

## `TransportPreference`

```rust
pub enum TransportPreference {
    Any,
    Http,
    UnixSocket,
    NamedPipe,
    InProcess,
}
```

Callers set `ClientConfig::preferred_transport` to whichever wire they
speak. `ClientConfig::for_current_project()` defaults to `Http`.

## `DaemonHandle`

```rust
pub struct DaemonHandle {
    pub pid: u32,
    pub address: TransportAddressInfo,
}

impl DaemonHandle {
    pub fn as_http(&self) -> Option<HttpEndpoints<'_>>;
}

pub struct HttpEndpoints<'a> {
    pub http_base: &'a str,
    pub ws_url: &'a str,
}
```

`address` carries the `TransportAddressInfo` variant the client
attached to. `as_http()` is a convenience that returns an
`HttpEndpoints<'_>` borrow when the daemon offered HTTP. Callers that
hold the URL beyond the lifetime of `DaemonHandle` should clone the
strings first.

## `TransportAddressInfo`

Locally duplicated from `daemon-core::transport::TransportAddressInfo`
to keep `daemon-client` free of a dep on `daemon-core`. Variants
match the daemon's serialization exactly:

```rust
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransportAddressInfo {
    Http { http_base: String, ws_url: String },
    UnixSocket { path: String },
    NamedPipe { path: String },
    InProcess,
}
```

## Ready file format

One on-disk format (`protocol_version: 1`) with a
`transports: Vec<TransportAddressInfo>` array. The reader
`serde_json::from_slice`s into `ReadyFileContent` directly. If the
schema ever breaks, bump `PROTOCOL_VERSION` in daemon-core's writer,
add a version check here in the reader, and handle the old shape
then.

## Binary resolution

`resolve_server_binary` tries in order:

1. `ClientConfig::server_binary` explicit override.
2. `$DAEMON_SERVER_BIN` environment variable.
3. `current_exe().parent().join("<daemon-binary>")` — the normal case
   when both binaries live in the same `target/debug/` or install
   prefix. The binary name is supplied by the caller via
   `ClientConfig`.

## Dependencies

- `fs4` — cross-platform advisory file locking.
- `ureq` — blocking HTTP client for the health probe.
- `anyhow`, `serde`, `serde_json`.

**No** runtime-core, daemon-core, or provider dependencies.
