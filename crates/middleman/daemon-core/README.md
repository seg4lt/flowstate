# daemon-core

Daemon lifecycle wiring. Everything needed to turn `runtime-core` +
`http-api` into a long-lived background process with idle
auto-shutdown, SIGINT handling, ready-file coordination, and graceful
shutdown.

## Entry points

### `bootstrap(bind_addr, database_name, project_root, frontend_dist, lifecycle)`

Constructs the full runtime: builds a tokio runtime, constructs every
provider adapter, opens SQLite, runs `reconcile_startup`, and spawns
an `http-api` server. Returns a `BootstrappedApp { tokio_runtime,
runtime_core, server }`. Used both by `run_blocking` below and by
in-process callers (e.g. dev tools) that want a real runtime without
the full daemon lifecycle.

The `lifecycle: Option<Arc<DaemonLifecycle>>` parameter toggles daemon
mode. `None` = fully in-process, no counters, no idle shutdown. `Some`
= wired to counters and ready for `run_blocking` below.

### `run_blocking(config: DaemonConfig)`

The daemon-binary entry point. Calls `bootstrap`, writes the ready
file, spawns `idle_watchdog` on the tokio runtime, installs a
SIGINT / ctrl-c handler, and blocks until either shutdown signal
fires. On wake, runs `graceful_shutdown`, drops the server and
runtime in the right order, and deletes the ready file.

## `DaemonLifecycle`

Atomic-counter state struct implementing both
`http_api::ConnectionObserver` (for client counts + shutdown requests +
`/api/status`) and `runtime_core::TurnLifecycleObserver` (for in-flight
turn count). The same `Arc<DaemonLifecycle>` gets upcast into both
trait objects and handed to `http-api` and `runtime-core` respectively
during `bootstrap`.

Also records `started_at` (`Instant` for uptime, RFC3339 string for
the status response) and `daemon_version` (from `CARGO_PKG_VERSION`)
so `/api/status` can serialize a complete snapshot.

## `idle_watchdog` task

Tokio task spawned from `run_blocking`. Waits for both counters
(`connected_clients` and `in_flight_turns`) to hit zero, then races
an `idle_timeout` (default 60s) against any new activity notification
or an explicit shutdown request. On timeout, fires the shutdown
oneshot. On activity, re-enters the wait loop.

## `ReadyFile`

Per-boot, per-project coordination file. Resolved via:

- macOS: `$TMPDIR/zenui/daemon-<hash>.json`
- Linux: `$XDG_RUNTIME_DIR/zenui/daemon-<hash>.json` (with `/tmp`
  fallback)
- Windows: `%LOCALAPPDATA%\zenui\daemon-<hash>.json`

Where `<hash>` is a `DefaultHasher` digest of the canonical project
root. Written atomically (`tmp` + `fsync` + `rename`) immediately
after the http-api server binds; deleted on graceful shutdown. Read
by [`../daemon-client/`](../daemon-client/README.md) to discover a
running daemon.

Contents:

```json
{
  "pid": 12345,
  "http_base": "http://127.0.0.1:53271",
  "ws_url": "ws://127.0.0.1:53271/ws",
  "protocol_version": 1,
  "started_at": "2026-04-11T22:26:26.459943+00:00",
  "daemon_version": "0.1.0",
  "project_root": "/path/to/project"
}
```

## Graceful shutdown sequence

1. Publish `RuntimeEvent::DaemonShuttingDown` so any attached client
   can surface a banner immediately.
2. Call `RuntimeCore::shutdown_all_turns(grace)` — loop-and-re-snapshot
   sweep of `active_sinks` via `interrupt_turn`, up to `grace` seconds
   (default 5). A new turn that slips in between phases is caught on
   the next iteration.
3. Return. The caller (`run_blocking`) drops the `LocalServer`,
   `RuntimeCore`, and tokio runtime in that order so subprocesses
   are reaped on a live runtime.

## Dependencies

- `runtime-core` — for `RuntimeCore`, `TurnLifecycleObserver`.
- `http-api` — for `spawn_local_server`, `ConnectionObserver`,
  `DaemonStatus`.
- `provider-api` — for `RuntimeEvent::DaemonShuttingDown`.
- `orchestration`, `persistence`, all five `provider-*` crates — for
  `bootstrap` to construct the full runtime.
- `directories`, `tokio`, `anyhow`, `chrono`, `serde`, `serde_json`,
  `tracing`, `tracing-subscriber`.
