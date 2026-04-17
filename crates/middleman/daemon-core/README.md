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

Three entry points, differing only in **who owns the tokio runtime**.
Pick by that axis first; the rest of the daemon-core lifecycle
(counters, ready file, graceful shutdown) is identical either way.

### Runtime ownership: app vs. daemon

| Scenario | Runtime | Entry point | Returns |
|---|---|---|---|
| Host app already has a tokio runtime (Tauri, axum, your own `#[tokio::main]`, a test harness) | **App owns it.** daemon-core borrows it. | `bootstrap_core_async` | `InProcessCore { runtime_core, lifecycle }` |
| Plain sync `main()` with no runtime yet (standalone daemon binary) | **daemon-core owns it.** Builds a multi-thread tokio runtime internally. | `bootstrap_core` | `BootstrappedCore { tokio_runtime, runtime_core, lifecycle }` |
| Full out-of-process daemon with transports, ready file, SIGINT, idle watchdog | daemon-core owns it (via `bootstrap_core`) | `run_blocking` | `Result<()>` (blocks until shutdown) |

**Rule of thumb:** if you already have a tokio runtime, use
`bootstrap_core_async`. Only reach for `bootstrap_core` when you
genuinely need the SDK to own a dedicated runtime.

### `bootstrap_core_async(config: &DaemonConfig) -> Result<InProcessCore>` — preferred for embedders

Async, **runtime-agnostic** bootstrap. Does NOT build a tokio runtime.
Call from inside an existing `#[tokio::main]`, a `tokio::test`, or a
spawned task. Every subsequent `RuntimeCore` call and every task it
spawns runs on the caller's runtime — no second runtime in the
process, no cross-runtime `block_on` hazards.

This is what flowstate does. The Tauri webview already drives a
multi-thread tokio runtime; flowstate hands that runtime to daemon-core
via `bootstrap_core_async` and holds the returned `InProcessCore` as
an application-wide state.

```rust
// Inside a Tauri setup handler, or any existing tokio context.
let core = zenui_daemon_core::bootstrap_core_async(&config)
    .await
    .context("failed to bootstrap agent runtime")?;
let runtime: Arc<RuntimeCore> = core.runtime_core;
// ... use runtime directly; spawn your own transport glue as needed.
```

Wires every enabled provider adapter, opens SQLite, constructs
`RuntimeCore` with `DaemonLifecycle` as its `TurnLifecycleObserver`,
reclaims sessions stuck at `Running` from a prior crash, and seeds the
provider-enablement map from persistence. Does NOT start any
transport — embedders call `RuntimeCore` methods directly (or wire
their own transport).

### `bootstrap_core(config: &DaemonConfig) -> Result<BootstrappedCore>` — sync wrapper

Thin sync wrapper over `bootstrap_core_async`. Builds its own
multi-thread tokio runtime (`thread_name = "zenui-runtime"`),
`block_on`s the async path, and returns a `BootstrappedCore` that
**owns** the runtime. Drop it to shut everything down.

Use from a plain sync `main()` that does **not** already have a
runtime — typically a standalone daemon binary.

> ⚠️ **Do not call this from inside an existing tokio runtime.** The
> internal `block_on` will panic with `Cannot start a runtime from
> within a runtime`. Reach for `bootstrap_core_async` instead.

### `run_blocking(config: DaemonConfig, transports: Vec<Box<dyn Transport>>) -> Result<()>`

Full daemon-binary entry point. Calls `bootstrap_core` (so it owns
its own runtime), binds and serves every transport, writes the ready
file, runs the idle watchdog + SIGINT handler, and coordinates
graceful shutdown. Sequence:

1. `bootstrap_core(&config)`.
2. For each transport, call `bind()` on the host thread. Errors abort
   startup.
3. Enter `tokio_runtime.block_on`. For each `Bound`, call `serve()`.
   On error, drain already-started handles via `shutdown()` before
   bubbling up.
4. **Write the ready file** (with the `transports: [...]` array)
   *after* every transport is serving. Invariant: ready file exists ⟹
   every listed transport is accepting connections.
5. Spawn `idle_watchdog`. Install SIGINT handler. Wait for shutdown.
6. On shutdown: publish `RuntimeEvent::DaemonShuttingDown`, call
   `graceful_shutdown` (which runs `shutdown_all_turns`), then drain
   every transport handle via `shutdown().await` in reverse order of
   start, delete the ready file, drop the runtime.

Library embedders typically do NOT call `run_blocking` — it's for the
standalone-daemon shape. Embedders stop at `bootstrap_core_async` and
wire their own application loop around the returned `RuntimeCore`.

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
what lets a daemon's `start` subcommand fail fast with a clear error
when the port is in use.

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

**Only `run_blocking` spawns the watchdog.** `bootstrap_core_async`
and `bootstrap_core` do not — embedders own their own application
lifetime (e.g. flowstate lives as long as the Tauri window does, and
explicitly calls `DaemonLifecycle::request_shutdown` on app exit).
If an embedder *wants* the watchdog, they can call `idle_watchdog`
from their own task after `bootstrap_core_async` returns.

`DaemonConfig::zero_transport(project_root)` sets `idle_timeout:
Duration::MAX` for embedded daemons — they'll never fire the idle
timer by themselves and rely on explicit
`DaemonLifecycle::request_shutdown`.

## `ReadyFile`

Per-boot, per-project coordination file at
`$TMPDIR/zenui/daemon-<hash>.json` (or platform equivalent). Written
atomically (`tmp` + `fsync` + `rename`) by `run_blocking` **after**
every transport is serving; deleted on graceful shutdown.

```json
{
  "pid": 12345,
  "protocol_version": 1,
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
