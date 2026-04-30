<p align="center">
  <img src="apps/flowstate/flowstate.jpg" alt="Flowstate" width="200" />
</p>

<h1 align="center">flowstate</h1>

<p align="center">
  A desktop app for Claude, Codex, GitHub Copilot, and OpenCode —
  with a shared agent runtime that lets sessions spawn and coordinate
  with each other.
</p>

---

## What it is

Flowstate is a Tauri app (Rust + React) that sits in front of the AI
coding agents you already use. It gives them a common chat UI, shared
session and project history, and a small set of tools that any agent
can call to start or talk to another agent.

**Providers**

- Claude — via Agent SDK or the `claude` CLI
- Codex — via the Codex CLI
- GitHub Copilot — via the Copilot SDK
- OpenCode

All four run through the same `ProviderAdapter` trait, so sessions,
history, and permissions behave the same regardless of provider.

## Agent orchestration

Every session is exposed to the runtime through a small MCP tool
surface (`mcp__flowstate__*`). An agent mid-turn can:

- `spawn` a new session (fire-and-forget) or `spawn_and_await` (block
  for a reply)
- `send` / `send_and_await` a message to an existing session
- `poll` or `read_session` to check on a peer without blocking
- `create_worktree` and `spawn_in_worktree` to provision an isolated
  git worktree and start a sub-agent inside it

The runtime enforces cycle detection on the await graph, a maximum
await depth, and a per-turn budget on orchestration calls — so
agents can delegate freely without deadlocking or fork-bombing.

A typical use: a Claude session spawns a Codex sub-session in a fresh
worktree to try a risky refactor, polls it for progress, and pulls
the result back into the main conversation when it's done.

## Install

Latest builds: [Releases](https://github.com/seg4lt/flowstate/releases/latest).

- **macOS (Apple Silicon)** — `Flowstate-<version>-macos-arm64.dmg`

Open the DMG and double-click **Install Flowstate.command** to copy
the app to `/Applications` and clear the quarantine flag.

- **Windows (x64)** — `Flowstate-<version>-windows-x64-setup.exe`
  (NSIS) or `Flowstate-<version>-windows-x64.msi` (Windows Installer).

Run either installer; the app installs per-user, no admin prompt.

The Linux build is still paused; its row is kept commented in
`.github/workflows/build.yml` and can be re-enabled without changes.

## Develop

```sh
cd apps/flowstate
pnpm install
pnpm tauri dev
```

Prereqs: Rust (stable), pnpm ≥ 10, bun, and the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## Build locally

The committed `tauri.conf.json` enables updater artifacts, which
require `TAURI_SIGNING_PRIVATE_KEY`. To produce a runnable `.app` /
`.dmg` locally without a signing key, override the bundle config:

```sh
cd apps/flowstate
pnpm tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

Outputs land in `target/release/bundle/`.

## Repo layout

- `apps/flowstate/` — Tauri desktop app (Rust + React)
- `crates/core/` — agent runtime, orchestrator, provider adapters,
  persistence (transport-agnostic)
- `crates/middleman/` — transport glue (Tauri IPC; HTTP+WS archived)
- `docs/screenshots/` — captured app screenshots
- `.github/workflows/build.yml` — tag-triggered release build

See [`apps/flowstate/README.md`](apps/flowstate/README.md) for
app-specific notes and [`crates/README.md`](crates/README.md) for
the Rust workspace.
