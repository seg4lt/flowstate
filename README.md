# flowstate

A desktop app for chatting with Claude, Codex, and GitHub Copilot from
one place.

## Install

Grab the latest build for your platform from
[Releases](https://github.com/seg4lt/flowstate/releases/latest):

- **macOS (Apple Silicon)** — `Flowstate-<version>-macos-arm64.dmg`
- **Windows (x64)** — `Flowstate-<version>-windows-x64.msi` or
  `-setup.exe`

On macOS, open the DMG and double-click **Install Flowstate.command**
to copy the app to `/Applications` and clear the quarantine flag.

## Develop

```sh
cd apps/flowstate
pnpm install
pnpm tauri dev
```

Prereqs: Rust (stable), pnpm ≥ 10, bun, and the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for
your platform.

## Repo layout

- `apps/flowstate/` — the Tauri desktop app (Rust + React)
- `crates/` — shared Rust crates: agent runtime, provider adapters,
  transports
- `.github/workflows/build.yml` — tag-triggered release build

See `apps/flowstate/README.md` for app-specific build notes and
`crates/README.md` for the Rust workspace.
