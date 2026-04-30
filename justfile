# Project task runner. Run `just` with no args to list available recipes.
#
# All Tauri commands run from `apps/flowstate` because that's where
# pnpm + tauri.conf.json live. The Rust workspace itself lives at the
# repo root, so `cargo` recipes run from here.

tauri_dir := "apps/flowstate"

# Default: list recipes.
default:
    @just --list

# Run the desktop app in dev mode (Vite + Tauri webview, hot reload).
dev:
    cd {{tauri_dir}} && pnpm tauri dev

# Local production build. Disables updater artifacts so the build
# works without TAURI_SIGNING_PRIVATE_KEY — handy for one-off local
# `.app` / `.exe` runs that won't ever be auto-updated.
local-build:
    cd {{tauri_dir}} && pnpm tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'

# Cross-compile the Rust workspace for Windows from macOS/Linux so
# `cfg(windows)` paths get type-checked locally before pushing. Uses
# cargo-zigbuild + zig (pinned in .mise.toml) for the linker; the
# windows-gnu target sidesteps the MSVC SDK requirement that the
# windows-msvc triple would impose. Catches the same `cfg(windows)`
# compile errors that break the GitHub Actions windows-latest job —
# saves a CI round-trip per fix.
#
# Mirrors CI ordering: build the flow CLI first and stage it as the
# Tauri externalBin sidecar (declared in tauri.windows.conf.json),
# then cross-compile the flowstate lib. Without the sidecar staged,
# tauri's build.rs aborts before reaching the Rust source.
#
# First time: `mise install && cargo install cargo-zigbuild && \
#   rustup target add x86_64-pc-windows-gnu`
build-windows:
    cargo zigbuild --release --target x86_64-pc-windows-gnu -p flow
    mkdir -p apps/flowstate/src-tauri/binaries
    cp target/x86_64-pc-windows-gnu/release/flow.exe \
       apps/flowstate/src-tauri/binaries/flow-x86_64-pc-windows-gnu.exe
    cargo zigbuild --release --target x86_64-pc-windows-gnu -p flowstate --lib
