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
# depend on any sibling DLL (WebView2Loader is statically linked) and
# a sibling `flow.exe` (the CLI sidecar that Tauri's externalBin
# system places next to the main exe at install time).
#
# Targets `x86_64-pc-windows-msvc` so the cfg_attr in
# `webview2-com-sys` activates the static-link path. The gnu ABI
# would force a dynamic dependency on `WebView2Loader.dll` — the
# crate's link declaration is `#[cfg_attr(target_env = "msvc",
# link(name = "WebView2LoaderStatic", kind = "static"))]`, gated to
# msvc-only.
#
# Cross-compiling MSVC from a non-Windows host requires the Microsoft
# C++ runtime and Windows SDK headers/libs; `xwin` downloads them
# from Microsoft's NuGet feed (one-time, ~600 MB under ~/.xwin) and
# `cargo-xwin` wires them into rustc's link path automatically.
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
    # 3. Frontend bundle. `pnpm tauri build` would normally drive
    # this via `beforeBuildCommand`, but we're invoking cargo
    # directly because Tauri's bundler can't cross-compile the
    # NSIS / MSI installers from macOS. Tauri's build.rs reads
    # apps/flowstate/dist to embed the assets into flowstate.exe.
    cd {{tauri_dir}} && pnpm install --frozen-lockfile && pnpm build
    # 4. flow CLI for Windows-MSVC. Built first so step 5's
    # externalBin check finds the staged sidecar at the path Tauri
    # expects.
    cargo xwin build --release --target x86_64-pc-windows-msvc -p flow
    mkdir -p apps/flowstate/src-tauri/binaries
    cp target/x86_64-pc-windows-msvc/release/flow.exe \
       apps/flowstate/src-tauri/binaries/flow-x86_64-pc-windows-msvc.exe
    # 5. flowstate.exe.
    #
    # WEBVIEW2_STATIC=1: webview2-com-sys's link declaration uses
    # the `WebView2LoaderStatic.lib` static archive instead of
    # `WebView2Loader.dll`, so the loader is embedded directly into
    # the exe. End-users still need the Microsoft Edge WebView2
    # Runtime installed (ships with Windows 11 + recent Windows 10) —
    # that's the *runtime component*, separate from the loader DLL.
    WEBVIEW2_STATIC=1 cargo xwin build --release --target x86_64-pc-windows-msvc -p flowstate
    @echo ""
    @echo "  ✓ Windows build ready — single-file exe, no sibling DLLs needed"
    @echo "    target/x86_64-pc-windows-msvc/release/flowstate.exe"
    @echo "    target/x86_64-pc-windows-msvc/release/flow.exe (CLI sidecar)"
