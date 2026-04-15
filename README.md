# flowstate

Tauri + React + TypeScript desktop app.

## Prerequisites

- [pnpm](https://pnpm.io/) ≥ 10
- [Rust](https://rustup.rs/) (stable toolchain)
- [bun](https://bun.sh/) — used by the agent SDK build scripts to compile the
  Claude SDK and GitHub Copilot bridges
- [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your
  platform (Xcode CLT on macOS, MSVC + WebView2 on Windows, webkit2gtk on
  Linux)

## Develop

```sh
pnpm install
pnpm tauri dev
```

## Release build

Build a release bundle for the current platform:

```sh
pnpm install
pnpm tauri build
```

Build explicitly for Apple Silicon (`aarch64-apple-darwin`):

```sh
pnpm tauri build --target aarch64-apple-darwin
```

Build artifacts land under:

- macOS arm: `src-tauri/target/aarch64-apple-darwin/release/bundle/`
  (`.app` under `macos/`, `.dmg` under `dmg/`)
- Default target: `src-tauri/target/release/bundle/`
  (`.deb` / `.AppImage` on Linux, `.msi` / `.exe` on Windows)

## Releases

`.github/workflows/build.yml` runs the same build via a platform matrix when
you push a tag matching `v*` (e.g. `v0.2.0`). Only the macOS arm target is
enabled today; the Windows and Linux entries are commented out and ready to
re-enable.

```sh
git tag v0.2.0
git push origin v0.2.0
```
