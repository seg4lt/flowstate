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

# Cross-compile the full Rust workspace + frontend for Windows from
# macOS/Linux. Produces a runnable flowstate.exe with flow.exe staged
# next to it (the Settings → Install CLI feature can find it via the
# `current_exe().parent()` lookup). Uses cargo-zigbuild + zig (pinned
# in .mise.toml) for the C linker; the windows-gnu target sidesteps
# the MSVC SDK requirement that the windows-msvc triple would impose.
# Same code path the GitHub Actions windows-latest job runs, just
# with the gnu ABI instead of msvc — useful for catching cfg(windows)
# compile errors locally without a CI round-trip.
#
# Recipe is idempotent and tolerant of fresh checkouts: it installs
# the toolchain, frontend deps, builds the Vite bundle, then the two
# Rust crates in CI order. Re-running it after a code edit only
# rebuilds what changed thanks to cargo + vite caches.
#
# Final artifacts (gnu ABI — this is for local smoke-testing only;
# real release artifacts come from CI's MSVC builds):
#   target/x86_64-pc-windows-gnu/release/flowstate.exe
#   target/x86_64-pc-windows-gnu/release/flow.exe
build-windows:
    # 1. Toolchain. `mise install` is a no-op when zig is already on
    # the pinned version; `cargo install --locked` is a no-op when
    # cargo-zigbuild is already installed at the same version;
    # `rustup target add` is a no-op when the target is already there.
    mise install
    cargo install --locked cargo-zigbuild
    rustup target add x86_64-pc-windows-gnu
    # 2. Frontend. `pnpm tauri build` would normally drive this via
    # `beforeBuildCommand`, but we're invoking cargo directly (Tauri's
    # bundler doesn't cross-compile the NSIS / MSI installers from
    # macOS), so we build the Vite bundle manually first. Tauri's
    # build.rs reads the dist dir to embed the assets into flowstate.
    cd {{tauri_dir}} && pnpm install --frozen-lockfile && pnpm build
    # 3. flow CLI for Windows. Built first so step 4's externalBin
    # check finds the staged sidecar.
    cargo zigbuild --release --target x86_64-pc-windows-gnu -p flow
    mkdir -p apps/flowstate/src-tauri/binaries
    cp target/x86_64-pc-windows-gnu/release/flow.exe \
       apps/flowstate/src-tauri/binaries/flow-x86_64-pc-windows-gnu.exe
    # 4. flowstate.exe. Build the binary (not just the lib) so the
    # output is a runnable Windows GUI executable.
    cargo zigbuild --release --target x86_64-pc-windows-gnu -p flowstate
    @echo ""
    @echo "  ✓ Windows build ready"
    @echo "    target/x86_64-pc-windows-gnu/release/flowstate.exe"
    @echo "    target/x86_64-pc-windows-gnu/release/flow.exe (sidecar)"
