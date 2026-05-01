//! GitHub Copilot bridge runtime.
//!
//! Mirror of `../provider-claude-sdk/src/bridge_runtime.rs` — see that
//! module for the full design rationale. Keep the two in sync when
//! changing staging / hydration semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$OUT_DIR/bridge-assets/"]
struct BridgeAssets;

// Load-bearing `include_str!`s: rustc tracks these in dep-info, so when
// `build.rs` rewrites either fingerprint file (content hash of the
// staged glue or deps changed) this module recompiles and the
// `#[derive(Embed)]` proc-macro re-scans `$OUT_DIR/bridge-assets/`
// with the fresh bytes.
#[allow(dead_code)]
const BRIDGE_GLUE_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-glue-fingerprint.txt"));
#[allow(dead_code)]
const BRIDGE_DEPS_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-deps-fingerprint.txt"));

/// Hash of the small glue files (`index.js`, `package.json`). Cheap to
/// rewrite in place — used for the `.glue-fp` sentinel inside the cache
/// dir so a code-only edit doesn't trigger a re-hydrate.
const BRIDGE_GLUE_FINGERPRINT: &str = env!("BRIDGE_GLUE_FINGERPRINT");
/// Hash of `package-lock.json` + the embedded Node version. Names the
/// cache dir. The ONLY thing that changes here triggers a fresh
/// `npm ci` (and hence a download) on first launch.
const BRIDGE_DEPS_FINGERPRINT: &str = env!("BRIDGE_DEPS_FINGERPRINT");

/// Sentinel filename inside the cache dir recording the GLUE_FP that
/// was last written. Keeps the in-place rewrite check O(1) — no need
/// to re-hash the embedded glue at runtime.
const GLUE_SENTINEL: &str = ".glue-fp";

#[derive(Debug, Clone)]
pub struct BridgeRuntime {
    pub dir: PathBuf,
    pub script: PathBuf,
}

static EXTRACTED: OnceLock<Result<BridgeRuntime, String>> = OnceLock::new();

pub fn ensure_bridge_available() -> Result<BridgeRuntime> {
    let cached = EXTRACTED.get_or_init(|| provision_once().map_err(|e| format!("{e:?}")));
    cached.clone().map_err(|e| anyhow!(e))
}

pub fn ensure_extracted() -> Result<BridgeRuntime> {
    ensure_bridge_available()
}

fn provision_once() -> Result<BridgeRuntime> {
    let cache_parent = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui");
    let cache_root = cache_parent.join(format!("copilot-bridge-{BRIDGE_DEPS_FINGERPRINT}"));
    let script = cache_root.join("index.js");
    let node_modules = cache_root.join("node_modules");
    let glue_sentinel = cache_root.join(GLUE_SENTINEL);

    // Best-effort orphan sweep BEFORE we provision the current dir.
    // Old `copilot-bridge-<prior-deps-fp>` siblings accumulate every
    // time the lockfile or Node version moves; if we don't reap them
    // the cache grows unbounded across releases (each ~100 MB+ of
    // native binaries). Done first so a failure here doesn't strand
    // the user's actual provisioning.
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
        // download.
        tracing::info!(
            deps_fp = BRIDGE_DEPS_FINGERPRINT,
            glue_fp = BRIDGE_GLUE_FINGERPRINT,
            cache = %cache_root.display(),
            "Copilot bridge: refreshing glue in place (deps unchanged)"
        );
        rewrite_glue_in_place(&cache_root)
            .with_context(|| format!("refresh glue in {}", cache_root.display()))?;
        write_glue_sentinel(&cache_root)?;
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

    // Cold path: deps dir doesn't exist or is half-provisioned.
    tracing::info!(
        deps_fp = BRIDGE_DEPS_FINGERPRINT,
        glue_fp = BRIDGE_GLUE_FINGERPRINT,
        cache = %cache_root.display(),
        "provisioning Copilot bridge"
    );

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
            "extracted Copilot bridge at {} is missing index.js",
            cache_root.display()
        );
    }

    if !node_modules.exists() {
        hydrate_node_modules(&cache_root)
            .with_context(|| format!("hydrate node_modules in {}", cache_root.display()))?;
    }

    // The Copilot package bundles per-platform native prebuilds (pty,
    // keytar, conpty, computer, clipboard) plus a vendored ripgrep.
    // npm preserves modes for its own install output, but the embed
    // path goes through rust-embed which strips them — belt-and-braces
    // chmod for both cases.
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
/// files (lockfile, node_modules, vendored binaries) are left
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
            "failed to stamp Copilot bridge glue sentinel; \
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

/// Delete sibling `copilot-bridge-<other>/` directories whose deps
/// fingerprint doesn't match the current build. Each old dir holds
/// ~100 MB+ of native prebuilds (pty, conpty, keytar, ripgrep), so
/// without this users accumulate dead bytes every release.
///
/// Best-effort: any errors are logged and swallowed. The current
/// provisioning is what matters; cleanup is bonus.
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
            "skipping Copilot bridge orphan sweep (debug build or ZENUI_SKIP_ORPHAN_SWEEP set)"
        );
        return;
    }
    let entries = match fs::read_dir(cache_parent) {
        Ok(e) => e,
        Err(_) => return,
    };
    let prefix = "copilot-bridge-";
    let current_suffix = BRIDGE_DEPS_FINGERPRINT;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        // Only touch dirs we own. Match `copilot-bridge-<hex>` and the
        // `.extracting` staging variant. Don't touch the
        // `claude-sdk-bridge-*` siblings — that's the other crate's
        // job.
        let is_ours = name_str.starts_with(prefix);
        if !is_ours {
            continue;
        }
        // Skip the active dir.
        if name_str == format!("{prefix}{current_suffix}") {
            continue;
        }
        // Skip the `.extracting` staging path that belongs to the
        // CURRENT provisioning attempt — provision_once cleans it
        // explicitly. Old `.extracting` dirs from prior aborted runs
        // ARE swept (they have a different deps_fp prefix).
        let active_staging = format!("{prefix}{current_suffix}.extracting");
        if name_str == active_staging {
            continue;
        }

        let path = entry.path();
        match fs::remove_dir_all(&path) {
            Ok(_) => tracing::info!(
                path = %path.display(),
                "swept orphaned Copilot bridge cache"
            ),
            Err(e) => tracing::debug!(
                path = %path.display(),
                error = %e,
                "failed to sweep Copilot bridge cache (likely in use; will retry next launch)"
            ),
        }
    }
}

fn extract_embedded_assets(staging: &Path) -> Result<()> {
    let file_count = BridgeAssets::iter().count();
    if file_count == 0 {
        anyhow::bail!("Copilot bridge assets are empty; rebuild with bridge/dist/index.js present");
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

/// Hydrate `cache_root/node_modules` for the Copilot bridge. See the
/// equivalent function in `provider-claude-sdk/src/bridge_runtime.rs`
/// for the full design rationale — short version: pnpm via corepack
/// for speed, transparent fallback to npm if anything goes wrong, the
/// produced node_modules layout is npm-compatible (hoisted) so the
/// bridge's `require()` calls don't care which path was taken.
fn hydrate_node_modules(cache_root: &Path) -> Result<()> {
    let node = zenui_embedded_node::ensure_available()
        .context("embedded Node is unavailable — cannot install bridge dependencies")?;

    if let Some(corepack_path) = locate_corepack(&node.bin_dir) {
        match hydrate_via_pnpm(&corepack_path, cache_root, &node.bin_dir) {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(
                    %err,
                    "pnpm hydration failed for Copilot bridge; falling back to npm"
                );
            }
        }
    }

    hydrate_via_npm(cache_root, &node.bin_dir)
}

fn hydrate_via_pnpm(corepack_path: &Path, cache_root: &Path, bin_dir: &Path) -> Result<()> {
    tracing::info!(
        cwd = %cache_root.display(),
        corepack = %corepack_path.display(),
        "hydrating Copilot bridge node_modules via corepack pnpm install --prod"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(corepack_path);
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.arg("pnpm")
        .arg("install")
        .arg("--prod")
        .arg("--reporter=append-only")
        .arg("--config.strict-peer-dependencies=false")
        .arg("--config.auto-install-peers=true")
        // Hoisted layout = npm-compatible flat node_modules; trades
        // some pnpm speed for compatibility with packages that walk
        // the tree expecting flat shape.
        .arg("--config.node-linker=hoisted")
        .arg("--config.store-dir")
        .arg(pnpm_store_dir()?)
        .current_dir(cache_root)
        // Suppress corepack's interactive download prompt — we have
        // no TTY in a GUI launch, so the prompt would deadlock.
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("PATH", prepend_path(bin_dir));

    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", corepack_path.display()))?;

    if !status.success() {
        anyhow::bail!("corepack pnpm install exited with {status}");
    }

    tracing::info!(
        duration_ms = started.elapsed().as_millis() as u64,
        "Copilot bridge node_modules hydrated via pnpm"
    );
    Ok(())
}

fn hydrate_via_npm(cache_root: &Path, bin_dir: &Path) -> Result<()> {
    let npm_path = locate_npm(bin_dir).ok_or_else(|| {
        anyhow!(
            "npm not found alongside embedded node at {}",
            bin_dir.display()
        )
    })?;

    // Prefer `npm ci` when a lockfile is staged alongside the
    // extracted bridge — exact versions, SRI-verified tarballs,
    // no drift. Fall back to `npm install` only when the lockfile
    // is absent (older builds or future escape hatch).
    let have_lockfile = cache_root.join("package-lock.json").exists();
    let install_subcommand = if have_lockfile { "ci" } else { "install" };

    tracing::info!(
        cwd = %cache_root.display(),
        npm = %npm_path.display(),
        mode = install_subcommand,
        "hydrating Copilot bridge node_modules via npm {install_subcommand} --omit=dev (fallback)"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(&npm_path);
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.arg(install_subcommand)
        .arg("--omit=dev")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--loglevel=error")
        // Match the Claude SDK bridge: warn-only on peer-dep
        // mismatches rather than ERESOLVE-fail, so a future
        // upstream-SDK peer bump doesn't brick first-launch.
        .arg("--legacy-peer-deps")
        .arg("--cache")
        .arg(npm_cache_dir()?)
        .current_dir(cache_root)
        .env("PATH", prepend_path(bin_dir));

    let status = cmd
        .status()
        .with_context(|| format!("spawn {} {}", npm_path.display(), install_subcommand))?;

    if !status.success() {
        anyhow::bail!(
            "npm {install_subcommand} for Copilot bridge failed with {status}; \
             try removing {} and re-launching, or build with `--features embed-all` \
             for an offline-ready binary",
            cache_root.display()
        );
    }

    tracing::info!(
        duration_ms = started.elapsed().as_millis() as u64,
        "Copilot bridge node_modules hydrated via npm"
    );
    Ok(())
}

/// Locate `corepack` next to `node`. Same Windows/Unix shim
/// situation as npm. Returns `None` for the very rare Node build
/// that strips corepack — caller falls back to npm.
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

/// Per-flowstate pnpm store. Shared across every bridge install in
/// this app so the Copilot bridge's first hydration can hardlink
/// any deps the Claude SDK bridge already pulled, and vice versa.
fn pnpm_store_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join("pnpm-store"))
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

fn npm_cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join("npm-cache"))
}

fn prepend_path(extra: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ';' } else { ':' };
    if existing.is_empty() {
        extra.display().to_string()
    } else {
        format!("{}{sep}{existing}", extra.display())
    }
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

/// Matches the vendored binaries the Copilot SDK spawns. Covers both
/// the ripgrep shipped under `@github/copilot/ripgrep/bin/<platform>/rg`
/// and the various `.node` native addons (pty, keytar, conpty, etc.)
/// that get loaded via `require()` — those don't need exec bits, but
/// the check below ignores them anyway (not matched by either clause).
#[cfg(unix)]
fn is_vendored_binary(path: &str) -> bool {
    (path.contains("/ripgrep/bin/") || path.contains("/vendor/ripgrep/"))
        && matches!(path.rsplit('/').next(), Some("rg") | Some("rg.exe"))
}
