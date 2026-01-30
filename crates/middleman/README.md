# middleman — transport, lifecycle, discovery

Wiring that bridges `crates/core/` to the outside world across process,
language, or network boundaries. Every crate here moves `ClientMessage`
/ `ServerMessage` over some kind of boundary without adding domain
behavior of its own.

## What lives here

- **[`http-api/`](./http-api/README.md)** — The `axum`-based HTTP +
  WebSocket transport. Binds loopback only, serves the React frontend,
  exposes the REST surface (`/api/bootstrap`, `/api/snapshot`,
  `/api/health`, `/api/status`, `/api/shutdown`), and streams
  `RuntimeEvent`s over `/ws`. Defines the `ConnectionObserver` trait
  that `daemon-core` hooks into.
- **[`daemon-core/`](./daemon-core/README.md)** — Daemon lifecycle:
  `bootstrap()`, `run_blocking()`, `DaemonLifecycle` counters,
  `idle_watchdog` task, `ReadyFile` coordination, graceful shutdown
  sequence. The in-process glue that turns `runtime-core` into a
  long-lived background process.
- **[`daemon-client/`](./daemon-client/README.md)** — Client-side
  daemon discovery. `connect_or_spawn()` reads the ready file,
  health-checks a running daemon, or auto-spawns one under an advisory
  lock. Deliberately depends on neither `runtime-core` nor
  `daemon-core` — this is the only crate a desktop shell needs to
  pull in to attach to a daemon.

## Dependency shape

```
http-api        ──►  core/runtime-core, core/provider-api
daemon-core     ──►  core/runtime-core, core/provider-api,
                      core/orchestration, core/persistence,
                      all five core/provider-*,
                      middleman/http-api
daemon-client   ──►  (nothing from core or middleman — it's a thin client)
```
