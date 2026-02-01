# middleman — transport, lifecycle, discovery

Wiring that bridges `crates/core/` to the outside world across process,
language, or network boundaries. Every crate here moves `ClientMessage`
/ `ServerMessage` over some kind of boundary without adding domain
behavior of its own.

## What lives here

- **[`daemon-core/`](./daemon-core/README.md)** — Daemon lifecycle +
  the `Transport` trait definition. `bootstrap_core()`,
  `run_blocking(config, transports)`, `DaemonLifecycle` counters,
  `idle_watchdog` task, `ReadyFile` coordination (v2 format with a
  `transports[]` array), graceful shutdown sequence. Defines the
  `Transport` / `Bound` / `TransportHandle` traits every transport
  crate implements.
- **[`transport-http/`](./transport-http/README.md)** — The
  `axum`-based HTTP + WebSocket transport. Implements
  `daemon_core::Transport` for `HttpTransport`. Exposes the existing
  REST surface (`/api/bootstrap`, `/api/snapshot`, `/api/health`,
  `/api/status`, `/api/shutdown`) and streams `RuntimeEvent`s over
  `/ws`. Serves the React frontend as static assets.
- **[`daemon-client/`](./daemon-client/README.md)** — Client-side
  daemon discovery. `connect_or_spawn()` reads the ready file (v1 and
  v2), filters by `preferred_transport`, health-checks a running
  daemon, or auto-spawns one under an advisory lock. Deliberately
  depends on neither `runtime-core` nor `daemon-core` — this is the
  only crate a desktop shell needs to pull in to attach to a daemon.

## Dependency shape

```
daemon-core     ──►  core/runtime-core, core/provider-api,
                      core/orchestration, core/persistence,
                      all five core/provider-*

transport-http  ──►  daemon-core (for Transport trait),
                      core/runtime-core, core/provider-api

daemon-client   ──►  (nothing from core or middleman — it's a thin client)
```

`daemon-core` does NOT depend on any transport crate. Transport crates
depend ON `daemon-core` for the trait. Apps are the composition root
and pull in whichever transports they want.
