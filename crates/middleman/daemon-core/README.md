# daemon-core

Daemon lifecycle wiring. Everything needed to turn `runtime-core` + a
caller-supplied set of transports into a long-lived background process
with idle auto-shutdown, SIGINT handling, ready-file coordination, and
graceful shutdown.

**Transport-agnostic.** `daemon-core` depends on `runtime-core`,
`provider-api`, `orchestration`, `persistence`, and every `provider-*`
crate — but NOT on any transport crate. Apps compose transports
explicitly and pass them to `run_blocking`. A GPUI or native CLI app
can use `bootstrap_core` or `run_blocking` with an empty transport
list, linking `daemon-core` without dragging in axum, hyper, or any
network stack.

## Entry points

### `bootstrap_core(config: &DaemonConfig) -> Result<BootstrappedCore>`

Pure headless bootstrap. Builds the tokio runtime, wires every provider
adapter, opens SQLite, constructs `RuntimeCore` with the
`DaemonLifecycle` as its `TurnLifecycleObserver`, and reconciles any
stuck sessions. Does not start any transport. Returns a
`BootstrappedCore { tokio_runtime, runtime_core, lifecycle }` for
embedded callers that want to drive the runtime in-process without
the full daemon loop.

### `run_blocking(config: DaemonConfig, transports: Vec<Box<dyn Transport>>) -> Result<()>`

The zenui-server binary entry point. Sequence:

1. `bootstrap_core(&config)`.
2. For each transport, call `bind()` on the host thread. Errors abort
   startup.
3. Enter `tokio_runtime.block_on`. For each `Bound`, call `serve()`.
   On error, drain already-started handles via `shutdown()` before
   bubbling up.
4. **Write the ready file v2** (with the `transports: [...]` array)
   *after* every transport is serving. Invariant: ready file exists ⟹
   every listed transport is accepting connections.
5. Spawn `idle_watchdog`. Install SIGINT handler. Wait for shutdown.
6. On shutdown: publish `RuntimeEvent::DaemonShuttingDown`, call
   `graceful_shutdown` (which runs `shutdown_all_turns`), then drain
   every transport handle via `shutdown().await` in reverse order of
   start, delete the ready file, drop the runtime.

## The `Transport` trait

Defined in `src/transport.rs`. Three traits + one enum:

```rust
pub trait Transport: Send {
    fn kind(&self) -> &'static str;
    fn bind(self: Box<Self>) -> Result<Box<dyn Bound>>;
}

pub trait Bound: Send {
    fn kind(&self) -> &'static str;
    fn address_info(&self) -> TransportAddressInfo;
    fn serve(
        self: Box<Self>,
        runtime: Arc<RuntimeCore>,
        observer: Arc<dyn ConnectionObserver>,
    ) -> Result<Box<dyn TransportHandle>>;
}

#[async_trait]
pub trait TransportHandle: Send + Sync {
    fn kind(&self) -> &'static str;
    fn address_info(&self) -> TransportAddressInfo;
    async fn shutdown(self: Box<Self>);
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransportAddressInfo {
    Http { http_base: String, ws_url: String },
    UnixSocket { path: String },
    NamedPipe { path: String },
    InProcess,
}
```

### Two-stage lifecycle rationale

**`bind()` is synchronous on the host thread.** This is the only place
a transport can claim OS resources (`StdTcpListener::bind`,
`UnixListener::bind`, `CreateNamedPipe`, ...). `bind()` must NOT call
any tokio API — tokio isn't running yet. Preserving this property is
what lets `zenui-server start` fail fast with a clear error when the
port is in use.

**`serve()` is synchronous but called inside tokio.** `tokio::spawn` is
a sync API, so marking `serve()` async would be misleading. The
returned `Box<dyn TransportHandle>` owns the spawned accept task plus
a shutdown oneshot.

**Only `shutdown()` is async** — it's the only method that needs to
await (drain outbound queues, wait for WebSocket close handshake, etc.).

## `DaemonLifecycle`

Atomic-counter state struct implementing both
`runtime_core::TurnLifecycleObserver` (for in-flight turn counting) and
`runtime_core::ConnectionObserver` (for client counting + shutdown
requests + `/api/status` snapshots). Same `Arc<DaemonLifecycle>` gets
upcast into both trait objects and handed to `RuntimeCore` and each
`Transport` respectively.

Also records `started_at` (an `Instant` for uptime + an RFC3339 string
for serialization) and `daemon_version` (`env!("CARGO_PKG_VERSION")`)
so `DaemonStatus` is a complete snapshot.

## `idle_watchdog`

Tokio task spawned from `run_blocking`. Waits for both
`connected_clients` and `in_flight_turns` to reach zero, then races
`idle_timeout` (default 60s) against new activity or explicit shutdown.
On timeout: fires the shutdown oneshot. On activity: re-enters the
wait loop.

`DaemonConfig::zero_transport(project_root)` sets `idle_timeout:
Duration::MAX` for embedded daemons — they'll never fire the idle
timer by themselves and rely on explicit `DaemonLifecycle::request_shutdown`.

## `ReadyFile` (v2 format)

Per-boot, per-project coordination file at
`$TMPDIR/zenui/daemon-<hash>.json` (or platform equivalent). Written
atomically (`tmp` + `fsync` + `rename`) by `run_blocking` **after**
every transport is serving; deleted on graceful shutdown.

```json
{
  "pid": 12345,
  "protocol_version": 2,
  "started_at": "2026-04-11T17:32:01Z",
  "daemon_version": "0.1.0",
  "project_root": "/path/to/project",
  "transports": [
    {
      "kind": "http",
      "http_base": "http://127.0.0.1:51705",
      "ws_url": "ws://127.0.0.1:51705/ws"
    }
  ]
}
```

Multi-transport daemons list every wire in the `transports[]` array.
`daemon-client` picks whichever transport its caller prefers.

## Graceful shutdown sequence

1. Publish `RuntimeEvent::DaemonShuttingDown` so attached clients can
   surface a banner immediately.
2. `RuntimeCore::shutdown_all_turns(grace)` — loop-and-re-snapshot
   sweep of `active_sinks` via `interrupt_turn`, up to `grace` seconds.
3. For each transport handle in reverse order, `shutdown().await` to
   drain connections.
4. Delete the ready file. Drop `RuntimeCore`, drop the tokio runtime.
   Subprocess providers are reaped as the runtime drops.

## Dependencies

- `runtime-core` — for `RuntimeCore`, `ConnectionObserver`,
  `DaemonStatus`, `TurnLifecycleObserver`.
- `provider-api` — for shared types and `RuntimeEvent`.
- `orchestration`, `persistence`, all five `provider-*` crates — for
  `bootstrap_core` to construct the full runtime.
- `directories`, `tokio`, `async-trait`, `anyhow`, `chrono`, `serde`,
  `serde_json`, `tracing`, `tracing-subscriber`.

**Not** a dep: any transport crate. `daemon-core` defines the trait;
transport crates depend *on* `daemon-core`, not the other way around.
