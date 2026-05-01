//! Claude SDK bridge runtime: make sure an extracted, npm-hydrated
//! copy of the bridge exists in the per-user cache before the adapter
//! spawns a Node.js child against it.
//!
//! Two compile-time modes, selected by the `embed` Cargo feature. Both
//! produce the same on-disk layout
//! (`~/.cache/zenui/claude-sdk-bridge-<fingerprint>/{index.js, package.json,
//! node_modules/…}`) and the spawn code that consumes [`BridgeRuntime`]
//! doesn't know which mode built it.
//!
//! - **`embed` ON** — `build.rs` stages the bridge glue AND the full
//!   `node_modules/` tree into `$OUT_DIR/bridge-assets/`. rust-embed
//!   bakes every file into the binary. First launch walks that tree,
//!   writes it to the cache, done. No network needed → fully offline.
//!
//! - **`embed` OFF (default)** — `build.rs` only stages the glue
//!   (`index.js` + `package.json` + `package-lock.json`, ~few KB).
//!   First launch writes those to the cache, then invokes
//!   `<embedded-node>/bin/npm ci --omit=dev` to hydrate
//!   `node_modules/` from npmjs.org. `npm ci` refuses to drift from
//!   the committed lockfile and validates each tarball against the
//!   SRI hashes inside it, so the dep set a user gets on first
//!   launch is bit-for-bit what we pinned. Subsequent launches hit
//!   the sentinel and return immediately — no network again.
//!
//! The lazy-hydration path trades a one-time ~30–90 s first-launch
//! install for avoiding ~200 MB of per-platform native-binary embedding
//! we'd otherwise have to bake in or host ourselves.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use rust_embed::Embed;

/// Every file staged into `$OUT_DIR/bridge-assets/` by `build.rs`,
/// embedded as static bytes in the compiled crate. In default (download)
/// mode this is ~3 small files; in `embed` mode it's the entire bridge
/// tree including `node_modules/`.
#[derive(Embed)]
#[folder = "$OUT_DIR/bridge-assets/"]
struct BridgeAssets;

/// Load-bearing `include_str!`s: rustc tracks these in dep-info, so
/// when `build.rs` rewrites either fingerprint file (content hash of
/// the staged glue or deps changed) this module recompiles and the
/// `#[derive(Embed)]` proc-macro above re-scans
/// `$OUT_DIR/bridge-assets/` with the fresh bytes. Without them a
/// `bun run build` could update the staged assets while the binary
/// kept shipping the previous build's bytes.
#[allow(dead_code)]
const BRIDGE_GLUE_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-glue-fingerprint.txt"));
#[allow(dead_code)]
const BRIDGE_DEPS_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-deps-fingerprint.txt"));

/// Hash of the small glue files (`index.js`, `package.json`). Cheap to
/// rewrite in place — used for the `.glue-fp` sentinel inside the cache
/// dir so a code-only edit doesn't trigger a re-hydrate.
///
/// Splitting glue from deps means a bridge-source edit no longer
/// invalidates `node_modules/`. Pre-split, every `bridge/src/index.ts`
/// tweak forced a fresh `npm ci` and ~100 MB of re-downloads on every
/// end-user's next launch.
const BRIDGE_GLUE_FINGERPRINT: &str = env!("BRIDGE_GLUE_FINGERPRINT");
/// Hash of `package-lock.json` + the embedded Node version. Names the
/// cache dir. The ONLY thing that changes here triggers a fresh
/// `npm ci` (and hence a download) on first launch.
const BRIDGE_DEPS_FINGERPRINT: &str = env!("BRIDGE_DEPS_FINGERPRINT");

/// Sentinel filename inside the cache dir recording the GLUE_FP that
/// was last written. Keeps the in-place rewrite check O(1) — no need
/// to re-hash the embedded glue at runtime.
const GLUE_SENTINEL: &str = ".glue-fp";

/// Resolved paths to an extracted Claude SDK bridge on disk.
#[derive(Debug, Clone)]
pub struct BridgeRuntime {
    /// Bridge working directory: contains `index.js`, `package.json`,
    /// and `node_modules/`. Pass this as the child process cwd so
    /// `require()` resolves correctly.
    pub dir: PathBuf,
    /// Absolute path to the bridge's entry point script.
    pub script: PathBuf,
}

static EXTRACTED: OnceLock<Result<BridgeRuntime, String>> = OnceLock::new();

/// Ensure the bridge is extracted AND its `node_modules/` populated,
/// running `npm install` if necessary. Runs at most once per process.
pub fn ensure_bridge_available() -> Result<BridgeRuntime> {
    let cached = EXTRACTED.get_or_init(|| provision_once().map_err(|e| format!("{e:?}")));
    cached.clone().map_err(|e| anyhow!(e))
}

/// Back-compat shim — existing spawn code calls `ensure_extracted()`.
pub fn ensure_extracted() -> Result<BridgeRuntime> {
    ensure_bridge_available()
}

fn provision_once() -> Result<BridgeRuntime> {
    let cache_parent = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui");
    let cache_root = cache_parent.join(format!("claude-sdk-bridge-{BRIDGE_DEPS_FINGERPRINT}"));
    let script = cache_root.join("index.js");
    let node_modules = cache_root.join("node_modules");
    let glue_sentinel = cache_root.join(GLUE_SENTINEL);

    // Best-effort orphan sweep BEFORE we provision the current dir.
    // Old `claude-sdk-bridge-<prior-deps-fp>` siblings accumulate
    // every time the lockfile or Node version moves; if we don't
    // reap them the cache grows unbounded across releases (each
    // ~100 MB+ of native binaries). Done first so a failure here
    // doesn't strand the user's actual provisioning.
    sweep_orphan_caches(&cache_parent);

    // Hot path: deps-dir is already populated, glue is up to date.
    // No filesystem work, no npm, no rust-embed walk.
    if script.exists() && node_modules.exists() {
        let sentinel_matches = fs::read_to_string(&glue_sentinel)
            .ok()
            .map(|s| s.trim() == BRIDGE_GLUE_FINGERPRINT)
            .unwrap_or(false);
        if sentinel_matches {
            return Ok(BridgeRuntime {
                dir: cache_root,
                script,
            });
        }

        // Warm path: deps unchanged but the glue (index.js /
        // package.json) was bumped. Rewrite ONLY those files in
        // place — node_modules is reused as-is, no `npm ci`, no
        // download. This is the whole point of splitting GLUE_FP
        // from DEPS_FP: a TS edit to `bridge/src/index.ts` no
        // longer triggers a ~100 MB re-download for every user.
        tracing::info!(
            deps_fp = BRIDGE_DEPS_FINGERPRINT,
            glue_fp = BRIDGE_GLUE_FINGERPRINT,
            cache = %cache_root.display(),
            "Claude SDK bridge: refreshing glue in place (deps unchanged)"
        );
        rewrite_glue_in_place(&cache_root)
            .with_context(|| format!("refresh glue in {}", cache_root.display()))?;
        write_glue_sentinel(&cache_root)?;
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

    // Cold path: deps dir doesn't exist or is half-provisioned
    // (missing node_modules means a prior first-launch was
    // interrupted mid-`npm install` or this is genuinely cold —
    // either way, re-hydrate).
    tracing::info!(
        deps_fp = BRIDGE_DEPS_FINGERPRINT,
        glue_fp = BRIDGE_GLUE_FINGERPRINT,
        cache = %cache_root.display(),
        "provisioning Claude SDK bridge"
    );

    // Extract the embedded glue into a staging dir and atomically
    // rename into place. node_modules hydration happens AFTER the
    // rename — installing into the final cache dir lets it survive
    // crashes during install (next run sees missing node_modules,
    // re-runs install in place; npm is idempotent).
    let staging = cache_root.with_extension("extracting");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;

    extract_embedded_assets(&staging)?;

    if cache_root.exists() {
        fs::remove_dir_all(&cache_root).ok();
    }
    if let Some(parent) = cache_root.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::rename(&staging, &cache_root)
        .with_context(|| format!("rename {} -> {}", staging.display(), cache_root.display()))?;

    if !script.exists() {
        anyhow::bail!(
            "extracted Claude SDK bridge at {} is missing index.js",
            cache_root.display()
        );
    }

    // Hydrate node_modules via npm when the embed didn't include it.
    if !node_modules.exists() {
        hydrate_node_modules(&cache_root)
            .with_context(|| format!("hydrate node_modules in {}", cache_root.display()))?;
    }

    // Restore +x on the vendored ripgrep that
    // `@anthropic-ai/claude-agent-sdk` spawns — rust-embed strips
    // Unix mode bits, and npm sometimes does too on macOS via certain
    // packagers. Cheap to always run, no-op when file is absent.
    #[cfg(unix)]
    chmod_vendored_binaries(&cache_root)?;

    write_glue_sentinel(&cache_root)?;

    Ok(BridgeRuntime {
        dir: cache_root,
        script,
    })
}

/// Overwrite ONLY the glue files (`index.js`, `package.json`) inside
/// an already-hydrated cache dir with their embedded versions. Other
/// files (lockfile, node_modules, vendored ripgrep) are left
/// untouched — they're determined by DEPS_FP, which by definition
/// hasn't changed if we're on this code path.
///
/// Per-file atomic-rename: write to `<name>.tmp` first, then rename
/// over the live file. A crash mid-rewrite leaves either the old
/// glue or the new one — never a half-written file the runtime
/// would try to parse and explode on.
fn rewrite_glue_in_place(cache_root: &Path) -> Result<()> {
    for name in ["index.js", "package.json"] {
        let embedded = BridgeAssets::get(name).ok_or_else(|| {
            anyhow!("embedded asset `{name}` missing — bridge build is broken")
        })?;
        let target = cache_root.join(name);
        let tmp = cache_root.join(format!("{name}.tmp"));
        if tmp.exists() {
            fs::remove_file(&tmp).ok();
        }
        fs::write(&tmp, embedded.data.as_ref())
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &target).with_context(|| {
            format!("rename {} -> {}", tmp.display(), target.display())
        })?;
    }
    Ok(())
}

/// Stamp the current GLUE_FP into the cache dir. The hot path reads
/// this on next launch to decide whether the in-place rewrite is
/// needed. Best-effort: the file is purely a cache hint, so a write
/// failure shouldn't fail provisioning.
fn write_glue_sentinel(cache_root: &Path) -> Result<()> {
    let path = cache_root.join(GLUE_SENTINEL);
    if let Err(e) = fs::write(&path, BRIDGE_GLUE_FINGERPRINT) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to stamp Claude SDK bridge glue sentinel; \
             next launch will rewrite glue unnecessarily but is otherwise correct"
        );
    }
    Ok(())
}

/// True when the orphan sweep should be skipped: debug builds (cargo
/// run / cargo test, where the binary fingerprint won't match the
/// user's installed app) or the `ZENUI_SKIP_ORPHAN_SWEEP` env-var
/// escape hatch (for release-mode test runs in CI).
fn should_skip_orphan_sweep() -> bool {
    if cfg!(debug_assertions) {
        return true;
    }
    std::env::var_os("ZENUI_SKIP_ORPHAN_SWEEP").is_some()
}

/// Delete sibling `claude-sdk-bridge-<other>/` directories whose deps
/// fingerprint doesn't match the current build. Each old dir holds
/// ~100 MB+ of `@anthropic-ai/claude-agent-sdk-<platform>` native
/// prebuilds + vendored ripgrep, so without this users accumulate
/// dead bytes every release.
///
/// Best-effort: any errors are logged and swallowed. The current
/// provisioning is what matters; cleanup is bonus. We only touch
/// `claude-sdk-bridge-*` siblings — the Copilot crate sweeps its
/// own `copilot-bridge-*` dirs on its own first-launch.
///
/// Skipped in dev/debug builds and when `ZENUI_SKIP_ORPHAN_SWEEP` is
/// set. A `cargo test` or `cargo run` build carries a different
/// fingerprint than the user's installed app build, so without this
/// guard a single test run on a dev box would delete the user's
/// production cache and force a multi-minute, ~100 MB+ re-install on
/// the next real launch. Release builds (the shipped app) still sweep.
fn sweep_orphan_caches(cache_parent: &Path) {
    if should_skip_orphan_sweep() {
        tracing::debug!(
            cache_parent = %cache_parent.display(),
            "skipping Claude SDK bridge orphan sweep (debug build or ZENUI_SKIP_ORPHAN_SWEEP set)"
        );
        return;
    }
    let entries = match fs::read_dir(cache_parent) {
        Ok(e) => e,
        Err(_) => return,
    };
    let prefix = "claude-sdk-bridge-";
    let current_suffix = BRIDGE_DEPS_FINGERPRINT;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let is_ours = name_str.starts_with(prefix);
        if !is_ours {
            continue;
        }
        if name_str == format!("{prefix}{current_suffix}") {
            continue;
        }
        // Skip the `.extracting` staging dir for the CURRENT
        // provisioning attempt — provision_once cleans it directly.
        // Old `.extracting` from prior aborted runs IS swept (its
        // deps_fp differs from the current one).
        let active_staging = format!("{prefix}{current_suffix}.extracting");
        if name_str == active_staging {
            continue;
        }

        let path = entry.path();
        match fs::remove_dir_all(&path) {
            Ok(_) => tracing::info!(
                path = %path.display(),
                "swept orphaned Claude SDK bridge cache"
            ),
            Err(e) => tracing::debug!(
                path = %path.display(),
                error = %e,
                "failed to sweep Claude SDK bridge cache (likely in use; will retry next launch)"
            ),
        }
    }
}

fn extract_embedded_assets(staging: &Path) -> Result<()> {
    let file_count = BridgeAssets::iter().count();
    if file_count == 0 {
        anyhow::bail!(
            "Claude SDK bridge assets are empty; rebuild with bridge/dist/index.js present"
        );
    }

    for path in BridgeAssets::iter() {
        let file = BridgeAssets::get(&path)
            .ok_or_else(|| anyhow!("embedded asset {path} vanished between iter and get"))?;
        let target = staging.join(&*path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
        fs::write(&target, file.data.as_ref())
            .with_context(|| format!("write {}", target.display()))?;
    }
    Ok(())
}

/// Hydrate `cache_root/node_modules` so the bridge can `require()`
/// `@anthropic-ai/claude-agent-sdk` and its per-platform optional-deps.
/// Blocks until the install finishes (caller is already on a
/// `spawn_blocking` thread via the daemon's `provision_runtimes`).
///
/// Tries pnpm via corepack first. Corepack ships inside Node ≥ 16.10
/// (the embedded Node is 20.11.1 so it's always present), and pnpm's
/// content-addressable global store + parallel fetcher is dramatically
/// faster than npm on cold installs — the difference users actually
/// notice on Windows where I/O is the bottleneck. We use pnpm with
/// `node-linker=hoisted` so the produced layout is npm-compatible
/// (flat `node_modules/`); the bridge's `require()` calls don't know
/// or care which package manager populated it.
///
/// On any pnpm failure (corepack download blocked, transient pnpm
/// bug, network glitch mid-install) we silently fall back to the
/// original npm path. Users only see "first launch is fast" or "first
/// launch is the speed it always was" — never a tooling failure.
fn hydrate_node_modules(cache_root: &Path) -> Result<()> {
    let node = zenui_embedded_node::ensure_available()
        .context("embedded Node is unavailable — cannot install bridge dependencies")?;

    // Corepack ships alongside `node` and `npm` in the embedded
    // Node's bin dir. `corepack` on Unix, `corepack.cmd` /
    // `corepack.exe` on Windows.
    if let Some(corepack_path) = locate_corepack(&node.bin_dir) {
        match hydrate_via_pnpm(&corepack_path, cache_root, &node.bin_dir) {
            Ok(()) => return Ok(()),
            Err(err) => {
                // Don't surface to the user — fall back transparently.
                // Logged at warn so it shows up in the daemon log if
                // we ever need to diagnose why a particular install
                // never benefits from the fast path.
                tracing::warn!(
                    %err,
                    "pnpm hydration failed; falling back to npm"
                );
            }
        }
    }

    hydrate_via_npm(cache_root, &node.bin_dir)
}

/// Run `corepack pnpm install --prod` in `cache_root`. pnpm's
/// content-addressable store lives under `pnpm_store_dir()` (a
/// flowstate-namespaced location so we don't scribble in the user's
/// personal `~/.local/share/pnpm`); subsequent installs hardlink
/// from there which is what makes pnpm fast on warm caches.
fn hydrate_via_pnpm(corepack_path: &Path, cache_root: &Path, bin_dir: &Path) -> Result<()> {
    tracing::info!(
        cwd = %cache_root.display(),
        corepack = %corepack_path.display(),
        "hydrating bridge node_modules via corepack pnpm install --prod"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(corepack_path);
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.arg("pnpm")
        .arg("install")
        .arg("--prod")
        // `--reporter=append-only` disables the live spinner so logs
        // captured by tracing (or CI) stay readable; fewer terminal
        // escapes, deterministic line-by-line output.
        .arg("--reporter=append-only")
        // Mimic npm's `--legacy-peer-deps` so a peer-dep mismatch in
        // a transitive dep doesn't brick first-launch. Bridge's
        // `package.json` already pins the concrete versions we need.
        .arg("--config.strict-peer-dependencies=false")
        .arg("--config.auto-install-peers=true")
        // Hoisted linker = flat node_modules just like npm produces.
        // pnpm's default symlink layout breaks packages that walk
        // node_modules expecting a specific shape (some optional
        // deps in the Agent SDK do this). Hoisted trades some pnpm
        // speed for maximum compatibility with arbitrary deps.
        .arg("--config.node-linker=hoisted")
        // Per-flowstate store so we don't depend on the user's pnpm
        // setup and don't pollute it either.
        .arg("--config.store-dir")
        .arg(pnpm_store_dir()?)
        .current_dir(cache_root)
        // Suppress corepack's interactive "do you want to download
        // pnpm?" prompt — we're a GUI process with no TTY, so the
        // prompt would deadlock. Newer corepack versions added this
        // env knob specifically for non-interactive callers.
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        // pnpm + corepack both expect `node` on PATH; pin PATH to
        // the embedded bin_dir so they can't pick up a stray system
        // node of a different major version.
        .env("PATH", prepend_path(bin_dir));

    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", corepack_path.display()))?;

    if !status.success() {
        anyhow::bail!("corepack pnpm install exited with {status}");
    }

    tracing::info!(
        duration_ms = started.elapsed().as_millis() as u64,
        "Claude SDK bridge node_modules hydrated via pnpm"
    );
    Ok(())
}

/// Original npm-based hydration path. Used as the silent fallback
/// when pnpm via corepack fails for any reason. Matches the
/// previous behavior 1:1 so no users regress on the slow path.
fn hydrate_via_npm(cache_root: &Path, bin_dir: &Path) -> Result<()> {
    let npm_path = locate_npm(bin_dir).ok_or_else(|| {
        anyhow!(
            "npm not found alongside embedded node at {}",
            bin_dir.display()
        )
    })?;

    // Prefer `npm ci` when a lockfile is staged alongside the
    // extracted bridge — it enforces exact versions, refuses to
    // drift, and verifies each tarball against the SRI hashes in
    // `package-lock.json`. Fall back to `npm install` only when
    // the lockfile is absent (older bridge builds, or a
    // future-proofing escape hatch for a broken lockfile).
    let have_lockfile = cache_root.join("package-lock.json").exists();
    let install_subcommand = if have_lockfile { "ci" } else { "install" };

    tracing::info!(
        cwd = %cache_root.display(),
        npm = %npm_path.display(),
        mode = install_subcommand,
        "hydrating bridge node_modules via npm {install_subcommand} --omit=dev (fallback)"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(&npm_path);
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.arg(install_subcommand)
        .arg("--omit=dev")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--loglevel=error")
        // Tolerate peer-dep conflicts in transitive deps. npm 7+
        // treats unresolved peers as a hard ERESOLVE error, which
        // would brick first-launch every time one of our pinned
        // bridge deps lags a peer-dep bump in the Claude Agent SDK.
        // Our bridge `package.json` already pins the concrete
        // versions we need, so falling back to the npm-6 behaviour
        // of warn-only is the safer default for end-user installs.
        .arg("--legacy-peer-deps")
        // Explicit cache dir under zenui namespace so we don't scribble
        // in the user's personal ~/.npm. Keeps offline cleanup simple.
        .arg("--cache")
        .arg(npm_cache_dir()?)
        .current_dir(cache_root)
        // npm expects `node` on PATH; pin PATH to the embedded bin_dir
        // so it can't pick up a stray system node that might be a
        // different major version.
        .env("PATH", prepend_path(bin_dir));

    let status = cmd
        .status()
        .with_context(|| format!("spawn {} {}", npm_path.display(), install_subcommand))?;

    if !status.success() {
        anyhow::bail!(
            "npm {install_subcommand} for Claude SDK bridge failed with {status}; \
             try removing {} and re-launching, or build with `--features embed-all` \
             for an offline-ready binary",
            cache_root.display()
        );
    }

    tracing::info!(
        duration_ms = started.elapsed().as_millis() as u64,
        "Claude SDK bridge node_modules hydrated via npm"
    );
    Ok(())
}

fn locate_npm(bin_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["npm.cmd", "npm"];
    #[cfg(not(windows))]
    let candidates = ["npm"];
    for name in candidates {
        let p = bin_dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Locate `corepack` next to `node`. Same Windows/Unix shim
/// situation as npm. Returns `None` on the (extremely rare) case
/// of a Node distribution that strips corepack — caller falls
/// back to npm.
fn locate_corepack(bin_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = ["corepack.cmd", "corepack.exe", "corepack"];
    #[cfg(not(windows))]
    let candidates = ["corepack"];
    for name in candidates {
        let p = bin_dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn npm_cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join("npm-cache"))
}

/// Per-flowstate pnpm store. pnpm hardlinks into this dir from every
/// bridge install, so a content-shared store across all our bridges
/// (claude-sdk, copilot, future ones) means each new bridge's first
/// install only fetches what's genuinely new — the rest of the deps
/// already exist on disk and get hardlinked in for free.
fn pnpm_store_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join("pnpm-store"))
}

fn prepend_path(extra: &Path) -> std::ffi::OsString {
    // Delegate to the workspace-shared helper so the user's
    // configured extra search dirs (`binaries.search_paths`) are
    // also visible to the hydration subprocesses (corepack, pnpm,
    // npm, and anything they fork like `git`). Without this the
    // hydration runs with the GUI's stripped PATH on Windows and
    // can't find tools the user added explicitly in Settings.
    zenui_provider_api::path_with_extras(&[extra])
}

#[cfg(unix)]
fn chmod_vendored_binaries(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !is_vendored_binary(&rel) {
                continue;
            }
            let mut perms = fs::metadata(&path)
                .with_context(|| format!("stat {}", path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)
                .with_context(|| format!("chmod +x {}", path.display()))?;
        }
    }
    Ok(())
}

/// Matches the vendored ripgrep that `@anthropic-ai/claude-agent-sdk`
/// spawns. npm install usually preserves modes, but rust-embed strips
/// them — this chmod is a belt-and-braces restore for the embed path.
#[cfg(unix)]
fn is_vendored_binary(path: &str) -> bool {
    path.contains("/vendor/ripgrep/")
        && matches!(path.rsplit('/').next(), Some("rg") | Some("rg.exe"))
}
