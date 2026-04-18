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

## Build locally

The committed `tauri.conf.json` enables updater artifacts, which
require `TAURI_SIGNING_PRIVATE_KEY` (used by CI for signed releases).
To produce a runnable `.app` / `.dmg` locally without the key, disable
updater artifacts via an inline config override:

```sh
cd apps/flowstate
pnpm tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

Outputs land in `target/release/bundle/` (`macos/flowstate.app` and
`dmg/flowstate_<version>_aarch64.dmg`). No `.tar.gz` / `.sig` pair is
produced, so no signing key is needed.

## Repo layout

- `apps/flowstate/` — the Tauri desktop app (Rust + React)
- `crates/` — shared Rust crates: agent runtime, provider adapters,
  transports
- `.github/workflows/build.yml` — tag-triggered release build

See `apps/flowstate/README.md` for app-specific build notes and
`crates/README.md` for the Rust workspace.
