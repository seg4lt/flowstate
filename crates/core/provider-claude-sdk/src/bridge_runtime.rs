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

/// Load-bearing `include_str!`: rustc tracks the file in dep-info, so
/// when `build.rs` rewrites it (content hash of the staged tree
/// changed) this module recompiles and the `#[derive(Embed)]` proc
/// macro above re-scans `$OUT_DIR/bridge-assets/` with the fresh
/// bytes. Without it a `bun run build` could update the staged
/// assets while the binary kept shipping the previous build's bytes.
#[allow(dead_code)]
const BRIDGE_ASSETS_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-assets-fingerprint.txt"));

/// Fingerprint emitted by `build.rs` via `cargo:rustc-env`. Also the
/// cache-directory namespace so upgrading the bridge (anything from
/// TS edits to a new lockfile) lands in a fresh dir rather than
/// overwriting a live one.
const BRIDGE_FINGERPRINT: &str = env!("BRIDGE_FINGERPRINT");

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
    let cache_root = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join(format!("claude-sdk-bridge-{BRIDGE_FINGERPRINT}"));
    let script = cache_root.join("index.js");
    let node_modules = cache_root.join("node_modules");

    // Sentinel: we only accept the cache as ready if BOTH the glue
    // and node_modules are present. Missing node_modules means either
    // a prior first-launch was interrupted mid-`npm install` or this
    // is genuinely cold — either way, re-hydrate.
    if script.exists() && node_modules.exists() {
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

    tracing::info!(
        fingerprint = BRIDGE_FINGERPRINT,
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

    Ok(BridgeRuntime {
        dir: cache_root,
        script,
    })
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

/// Run `<embedded-node>/bin/npm ci --omit=dev` (or `install` as a
/// fallback when the lockfile is absent) in `cache_root` to fetch
/// `@anthropic-ai/claude-agent-sdk` and its per-platform optional-deps
/// from npmjs.org into `cache_root/node_modules`. Blocks until the
/// install finishes (caller is already on a spawn_blocking thread via
/// the daemon's `provision_runtimes`).
fn hydrate_node_modules(cache_root: &Path) -> Result<()> {
    let node = zenui_embedded_node::ensure_available()
        .context("embedded Node is unavailable — cannot run npm install")?;

    // `npm` on Unix is a shim script that shebangs to node. On Windows
    // the shim is `npm.cmd`. Both live alongside `node` in bin_dir.
    let npm_path = locate_npm(&node.bin_dir)
        .ok_or_else(|| anyhow!("npm not found alongside embedded node at {}", node.bin_dir.display()))?;

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
        "hydrating bridge node_modules via npm {install_subcommand} --omit=dev"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(&npm_path);
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
        .env("PATH", prepend_path(&node.bin_dir));

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
        "Claude SDK bridge node_modules hydrated"
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

/// Matches the vendored ripgrep that `@anthropic-ai/claude-agent-sdk`
/// spawns. npm install usually preserves modes, but rust-embed strips
/// them — this chmod is a belt-and-braces restore for the embed path.
#[cfg(unix)]
fn is_vendored_binary(path: &str) -> bool {
    path.contains("/vendor/ripgrep/")
        && matches!(path.rsplit('/').next(), Some("rg") | Some("rg.exe"))
}
