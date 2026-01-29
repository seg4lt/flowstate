# ZenUI

A multi-provider AI coding agent shell. ZenUI is a native desktop app that
orchestrates conversations with Claude (SDK + CLI), Codex, and GitHub
Copilot (SDK + CLI), backed by a background daemon so long-running agent
turns survive closing the window.

---

## Architecture

```
                          ┌─────────────────────────────────────────┐
                          │              zenui-server               │
                          │                  (daemon)               │
                          │                                         │
  ┌─────────────┐         │   ┌─────────────────────────────────┐   │
  │  zenui      │◄───WS───┼──►│  http-api (axum, loopback only) │   │
  │  (desktop   │         │   │  /api/bootstrap /api/snapshot   │   │
  │  shell,     │         │   │  /api/status    /api/shutdown   │   │
  │  tao+wry)   │◄──HTTP──┼──►│  /ws   (streams RuntimeEvents)  │   │
  └─────────────┘         │   └────────────────┬────────────────┘   │
         ▲                │                    │                    │
         │                │                    ▼                    │
         │                │   ┌─────────────────────────────────┐   │
         │                │   │           RuntimeCore           │   │
         │                │   │  • orchestration + sessions     │   │
         │                │   │  • broadcast event bus          │   │
         │                │   │  • ClientMessage/ServerMessage  │   │
         │                │   └────────────────┬────────────────┘   │
         │                │                    │                    │
         │                │        ┌───────────┼───────────┐        │
         │                │        ▼           ▼           ▼        │
         │                │    ┌──────┐   ┌────────┐   ┌────────┐   │
         │                │    │ SQL  │   │provider│   │provider│   │
         │                │    │ite   │   │-claude │   │-codex  │   │
         │                │    │(persist)│ │-sdk    │   │...     │   │
         │                │    └──────┘   └───┬────┘   └───┬────┘   │
         │                │                   │            │        │
         │                └───────────────────┼────────────┼────────┘
         │                                    ▼            ▼
         │                              ┌──────────┐  ┌──────────┐
         │                              │  claude  │  │  codex   │
         │                              │  CLI /   │  │  CLI     │
         │                              │  bridge  │  │          │
         │                              └──────────┘  └──────────┘
         │
         │  Auto-spawn coordination via ready file at
         │  $TMPDIR/zenui/daemon-{hash(project_root)}.json
         │
         ▼
  [ user launches `zenui` ]
       │
       ▼
  1. daemon-client reads the ready file
  2. if healthy → attach
  3. if absent  → fork-exec `zenui-server start`, wait for ready file
  4. open webview pointed at http_base
```

### Lifecycle invariants

- **Sessions survive closing the window.** In-flight provider turns run
  inside the daemon's tokio runtime; closing `zenui` only decrements the
  daemon's `connected_clients` counter.
- **Idle auto-shutdown.** When both `connected_clients` and
  `in_flight_turns` are zero, an idle timer (default 60s) starts. Any
  new connection or turn cancels it.
- **Per-project isolation.** The ready file is keyed by a hash of the
  canonical project root, so two projects run independent daemons with
  their own SQLite databases.
- **Crash recovery.** `RuntimeCore::reconcile_startup` walks persisted
  sessions on boot and marks any stuck `Running` sessions as
  `Interrupted`, preventing the UI from spinning forever after a daemon
  crash.

---

## Crate Layout

```
zenui/
├── apps/
│   └── zenui/
│       ├── crate/
│       │   └── server/              zenui-server binary (daemon)
│       │
│       └── main-application/        zenui binary (desktop shell)
│                                    owns build.rs for the React frontend
│
├── crates/
│   ├── core/                        Shared domain — the product capability
│   │   ├── runtime-core              Event bus + orchestration dispatch
│   │   ├── provider-api              Shared trait + ClientMessage/ServerMessage
│   │   ├── orchestration             Session / turn state machine
│   │   ├── persistence               SQLite (bundled rusqlite)
│   │   ├── provider-claude-sdk       Concrete adapter: Claude SDK bridge
│   │   ├── provider-claude-cli       Concrete adapter: Claude CLI
│   │   ├── provider-codex            Concrete adapter: Codex
│   │   ├── provider-github-copilot       GitHub Copilot SDK
│   │   └── provider-github-copilot-cli   GitHub Copilot CLI
│   │
│   └── middleman/                   Shared wire / lifecycle / discovery
│       ├── http-api                  axum HTTP+WS transport
│       ├── daemon-core               Lifecycle state, idle watchdog,
│       │                             ready file, graceful shutdown
│       └── daemon-client             Discovery + auto-spawn + health check
│
├── frontend/                        React 19 + Vite + Tailwind 4 + shadcn
├── scripts/
│   └── smoke-daemon.sh              End-to-end daemon lifecycle test
└── README.md                        (this file)
```

### Dependency direction

```
        apps/*
          │
          ▼
  crates/middleman/*
          │
          ▼
    crates/core/*
```

Apps depend on middleman and (transitively) on core. Middleman depends on
core. Core never depends on middleman or apps. There are no cycles.

### Binaries

| Binary         | Crate                               | Role                                    |
| -------------- | ----------------------------------- | --------------------------------------- |
| `zenui`        | `apps/zenui/main-application`       | Desktop shell (tao + wry webview)       |
| `zenui-server` | `apps/zenui/crate/server`           | Daemon: owns runtime, providers, SQLite |

---

## Build

### Prerequisites

- **Rust** — edition 2024 requires rustc 1.85+.
- **Bun** — used by the frontend build at `frontend/` (the `apps/zenui/crate/server/build.rs`
  script invokes `bun install` and `bun run build`).
- **Node.js** is NOT required — some providers download their own Node at
  build time for isolated TypeScript bridges.

### Build everything

```bash
# From the repo root.
cargo build --workspace
```

This produces:

- `target/debug/zenui` — the desktop shell
- `target/debug/zenui-server` — the daemon
- `frontend/dist/` — the built React app (served by the daemon)

To skip the frontend rebuild on subsequent compile iterations, set
`ZENUI_SKIP_FRONTEND_BUILD=1`:

```bash
ZENUI_SKIP_FRONTEND_BUILD=1 cargo check --workspace
```

### Run

```bash
# Launch the desktop shell. Auto-spawns zenui-server if not already running.
./target/debug/zenui

# Or run the daemon directly in the foreground, logging to stderr.
./target/debug/zenui-server start --foreground

# Check daemon status (prints ready file contents + live counters).
./target/debug/zenui-server status

# Ask the running daemon to shut down gracefully.
./target/debug/zenui-server stop
```

The daemon discovers a per-project ready file at
`$TMPDIR/zenui/daemon-<hash>.json` (macOS / Linux) or
`%LOCALAPPDATA%\zenui\daemon-<hash>.json` (Windows). Run `zenui` or
`zenui-server` from the project root you want the daemon scoped to, or
pass `--project-root <PATH>` to the server commands.

### Test

```bash
# Rust unit + integration tests.
ZENUI_SKIP_FRONTEND_BUILD=1 cargo test --workspace

# End-to-end daemon lifecycle smoke test.
./scripts/smoke-daemon.sh
```

The smoke test starts `zenui-server`, confirms the ready file, probes
`/api/health` and `/api/status` over HTTP, posts `/api/shutdown`, and
asserts the daemon exits cleanly.

---

## Key Files

- `crates/core/runtime-core/src/lib.rs` — `RuntimeCore::send_turn`,
  `handle_client_message`, `reconcile_startup`, `shutdown_all_turns`,
  and the `TurnLifecycleObserver` trait.
- `crates/core/provider-api/src/lib.rs` — `ClientMessage`,
  `ServerMessage`, `RuntimeEvent`, and the `ProviderAdapter` trait.
- `crates/middleman/http-api/src/lib.rs` — axum router, `ws_handler`,
  `ConnectionObserver` trait, and the `/api/shutdown` + `/api/status`
  endpoints.
- `crates/middleman/daemon-core/src/lifecycle.rs` — `DaemonLifecycle`
  struct, atomic counters, and the `idle_watchdog` task.
- `crates/middleman/daemon-core/src/lib.rs` — `bootstrap()` and
  `run_blocking()` entry points.
- `crates/middleman/daemon-client/src/lib.rs` — `connect_or_spawn()`,
  `DaemonHandle`, and the fs4-based spawn lock.
- `apps/zenui/main-application/src/main.rs` — tao + wry window code,
  attaches to the daemon via `connect_or_spawn`.
- `apps/zenui/crate/server/src/main.rs` — `zenui-server` clap entry
  point with `start` / `stop` / `status` subcommands.
