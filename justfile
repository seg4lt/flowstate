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
# macOS/Linux. Produces a single-file `flowstate.exe` that doesn't
# depend on any sibling DLL (WebView2Loader + the C runtime are both
# statically linked) and a sibling `flow.exe` (the CLI sidecar that
# Tauri's externalBin system places next to the main exe at install
# time).
#
# Drives `pnpm tauri build` exactly the way CI does — same flags,
# same env, same code paths through the Tauri CLI — so any divergence
# from the windows-x64 matrix row is a bug. The only difference is
# the runner: locally we point Tauri at `cargo-xwin` (which provides
# the MSVC SDK headers + libs that aren't on macOS), and we pass
# `--no-bundle` because Tauri's NSIS/MSI bundlers don't cross-compile
# from macOS hosts. CI runs natively on `windows-latest` so it skips
# both of those workarounds.
#
# Targets `x86_64-pc-windows-msvc` so webview2-com-sys's static-link
# branch activates (it's gated `cfg_attr(target_env = "msvc", ...)`).
# The MSVC SDK is downloaded from Microsoft's NuGet feed via `xwin`
# (one-time, ~600 MB under ~/.xwin) and reused across builds.
#
# Recipe is idempotent and tolerant of fresh checkouts: every step
# is a no-op when its outputs already exist. Re-running after a code
# edit only rebuilds what changed.
#
# Final artifacts (single-file, no DLL alongside required):
#   target/x86_64-pc-windows-msvc/release/flowstate.exe
#   target/x86_64-pc-windows-msvc/release/flow.exe
build-windows:
    # 1. Toolchain — all idempotent; cargo install / rustup / mise
    # short-circuit when the requested version is already present.
    mise install
    cargo install --locked cargo-xwin xwin
    rustup target add x86_64-pc-windows-msvc
    # 2. MSVC SDK headers + libs from Microsoft's NuGet feed. Skip
    # if already extracted under ~/.xwin (the splat is ~600MB so
    # we don't redo it on every build).
    test -d ~/.xwin/crt && test -d ~/.xwin/sdk || \
        xwin --accept-license splat --output ~/.xwin
    # 3. Frontend deps. `pnpm tauri build` runs the Vite bundle
    # itself via `beforeBuildCommand` (configured in
    # tauri.conf.json), so we don't need a separate `pnpm build`
    # step here — but pnpm install IS a prerequisite.
    cd {{tauri_dir}} && pnpm install --frozen-lockfile
    # 4. flow CLI for Windows-MSVC. Built first so step 5's
    # externalBin check finds the staged sidecar at the path Tauri
    # expects.
    cargo xwin build --release --target x86_64-pc-windows-msvc -p flow
    mkdir -p apps/flowstate/src-tauri/binaries
    cp target/x86_64-pc-windows-msvc/release/flow.exe \
       apps/flowstate/src-tauri/binaries/flow-x86_64-pc-windows-msvc.exe
    # 5. Tauri's native build pipeline.
    #
    # `--runner cargo-xwin`: instead of invoking plain `cargo build`,
    # Tauri runs `cargo-xwin build` — picks up the MSVC SDK paths
    # automatically. The Tauri CLI still adds `--features
    # tauri/custom-protocol`, the right `--profile`, the right
    # target triple flags, etc., the same way it does for CI's
    # native MSVC build.
    #
    # `--no-bundle`: skips the NSIS / MSI / updater bundling steps.
    # Tauri's bundler tooling for those formats doesn't cross-
    # compile from macOS, and the produced exe is what we actually
    # want for local smoke-testing anyway. CI's `windows-latest`
    # runner runs the bundler natively and produces the installers.
    #
    # `WEBVIEW2_STATIC=1`: webview2-com-sys's link declaration uses
    # `WebView2LoaderStatic.lib` instead of dynamically linking
    # `WebView2Loader.dll`. Combined with `+crt-static` from
    # `.cargo/config.toml`, the resulting exe has no
    # `vcruntime140.dll` / `WebView2Loader.dll` dependencies. CI
    # gets the same flag from `.github/workflows/build.yml`'s env
    # block; this line keeps local + CI in sync.
    cd {{tauri_dir}} && WEBVIEW2_STATIC=1 pnpm tauri build \
        --runner cargo-xwin \
        --target x86_64-pc-windows-msvc \
        --no-bundle
    @echo ""
    @echo "  ✓ Windows build ready — single-file exe, no sibling DLLs needed"
    @echo "    target/x86_64-pc-windows-msvc/release/flowstate.exe"
    @echo "    target/x86_64-pc-windows-msvc/release/flow.exe (CLI sidecar)"
