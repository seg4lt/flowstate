# flowstate

A multi-provider AI coding agent desktop app. Flowstate orchestrates
conversations with Claude (SDK + CLI), Codex, and GitHub Copilot (SDK +
CLI) from a single native window, with the agent runtime embedded
directly in-process.

---

## Repo layout

```
flowstate/
├── apps/
│   └── flowstate/                  Tauri desktop app (Rust + React)
│       ├── src/                    React 19 + Vite + Tailwind 4 + shadcn UI
│       └── src-tauri/              Tauri shell + in-process agent runtime
│
├── crates/
│   ├── core/                       Shared domain — the product capability
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
│       ├── daemon-core               Transport trait + lifecycle state,
│       │                             idle watchdog, ready file v2,
│       │                             graceful shutdown
│       ├── transport-tauri           In-process Tauri IPC transport
│       │                             (used by apps/flowstate)
│       ├── transport-http            axum HTTP+WS transport (pure transport;
│       │                             available for out-of-process daemons)
│       ├── daemon-client             Discovery + auto-spawn + health check
│       └── embedded-node             Embedded Node.js runtime for CLI bridges
│
├── scripts/                        Build / release helpers
└── .github/workflows/              CI: tag-triggered Tauri release build
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

Apps depend on middleman and (transitively) on core. Middleman depends
on core. Core never depends on middleman or apps. There are no cycles.

---

## Build

### Prerequisites

- **Rust** — edition 2024 requires rustc 1.85+.
- **Bun** or **pnpm** — used by the flowstate frontend at
  `apps/flowstate/src/`.
- **Node.js** is NOT required at the repo level — some providers
  download their own Node at build time for isolated TypeScript
  bridges.

### Build the desktop app

```bash
# Frontend deps (from apps/flowstate/)
pnpm install

# Dev mode (Vite + Tauri with hot reload)
pnpm tauri dev

# Release build (native installer + updater artifacts)
pnpm tauri build
```

### Build the workspace crates

```bash
# From the repo root.
cargo build --workspace
```

---

## Release

Tag-triggered release via `.github/workflows/build.yml`:

```bash
git tag v0.x.y
git push --tags
```

The workflow builds macOS (arm64) and Windows (x64) installers,
generates a Tauri updater manifest (`latest.json`), and publishes a
GitHub release on this repo. Requires the `TAURI_SIGNING_PRIVATE_KEY`
secret in GitHub Actions — everything else uses the default
`GITHUB_TOKEN`.

---

## Key files

- `apps/flowstate/src-tauri/src/main.rs` — Tauri entry point, wires the
  in-process agent runtime into Tauri IPC.
- `crates/core/runtime-core/src/lib.rs` — `RuntimeCore::send_turn`,
  `handle_client_message`, `reconcile_startup`, `shutdown_all_turns`,
  and the `TurnLifecycleObserver` trait.
- `crates/core/provider-api/src/lib.rs` — `ClientMessage`,
  `ServerMessage`, `RuntimeEvent`, and the `ProviderAdapter` trait.
- `crates/middleman/daemon-core/src/lib.rs` — `bootstrap()` and
  `run_blocking()` entry points, plus the `Transport` trait.
- `crates/middleman/transport-tauri/` — the transport flowstate uses.
- `crates/middleman/transport-http/` — pure HTTP+WS transport,
  available for out-of-process daemon deployments.
