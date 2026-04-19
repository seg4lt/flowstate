# middleman — transport, lifecycle, discovery

Wiring that bridges `crates/core/` to the outside world across process,
language, or network boundaries. Every crate here moves `ClientMessage`
/ `ServerMessage` over some kind of boundary without adding domain
behavior of its own.

## What lives here

- **[`daemon-core/`](./daemon-core/README.md)** — Daemon lifecycle +
  the `Transport` trait re-export. `bootstrap_core_async()`,
  `DaemonLifecycle` counters, `idle_watchdog` task, `ReadyFile`
  coordination (v2 format with a `transports[]` array), graceful
  shutdown sequence. Re-exports the `Transport` / `Bound` /
  `TransportHandle` traits from `runtime-core` so transport crates
  can implement them without a middleman dep cycle. The sync
  `bootstrap_core` + `run_blocking` entry points (for a standalone
  daemon binary driven from a sync `main`) sit behind the
  `standalone-binary` feature flag.
- **[`transport-tauri/`](./transport-tauri/README.md)** — In-proc
  transport for Tauri apps. Implements `daemon_core::Transport`
  over Tauri's host `Channel<T>` so messages and events never
  leave the process. Opted in via the `transport-tauri` feature of
  `daemon-core`.

### Dropped in Phase 6.1

The HTTP + WebSocket transport (`transport-http`) and the client-side
daemon-discovery crate (`daemon-client`) were removed from the
workspace — no in-tree consumer used them. The code lives in git
history if a future binary wants an external HTTP surface or
auto-spawn client; reintroduce by re-adding to `[workspace.members]`
and wiring optional deps in `daemon-core`.

## Dependency shape

```
daemon-core     ──►  core/runtime-core, core/provider-api,
                      core/persistence
                  (optional) middleman/transport-tauri

transport-tauri ──►  core/runtime-core (for the Transport trait),
                      core/provider-api
```

`daemon-core` does NOT depend on any transport crate except through
optional features. Transport crates depend on `runtime-core` for the
trait. Apps are the composition root and pull in whichever transports
they want (flowstate: just `transport-tauri`).
