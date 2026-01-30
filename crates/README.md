# crates/ — shared Rust crates

Library crates used across ZenUI's apps. Organized into two layers:

- **[`core/`](./core/README.md)** — the domain. Everything that encodes
  what ZenUI *does*: the runtime, session orchestration, persistence,
  the shared protocol, and the provider adapters that talk to external
  AI processes. Transport-agnostic — these crates don't know whether
  they're running inside a daemon, a CLI, or a test harness.

- **[`middleman/`](./middleman/README.md)** — the wiring that bridges
  `core/` to the outside world: HTTP + WebSocket transport, daemon
  lifecycle + idle watchdog, client-side discovery and auto-spawn. Each
  crate here moves `ClientMessage` / `ServerMessage` across some boundary
  (network, process, transport).

## Dependency direction

```
    apps/*          ← binaries in ../apps/
      │
      ▼
 middleman/*
      │
      ▼
    core/*
```

Apps depend on middleman and (transitively) on core. Middleman depends
on core. Core never depends on middleman or apps. There are no cycles.
