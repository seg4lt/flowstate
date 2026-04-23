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

#[allow(dead_code)]
const BRIDGE_ASSETS_FINGERPRINT_FILE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-assets-fingerprint.txt"));

const BRIDGE_FINGERPRINT: &str = env!("BRIDGE_FINGERPRINT");

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
    let cache_root = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join(format!("copilot-bridge-{BRIDGE_FINGERPRINT}"));
    let script = cache_root.join("index.js");
    let node_modules = cache_root.join("node_modules");

    if script.exists() && node_modules.exists() {
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

    tracing::info!(
        fingerprint = BRIDGE_FINGERPRINT,
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

    Ok(BridgeRuntime {
        dir: cache_root,
        script,
    })
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

fn hydrate_node_modules(cache_root: &Path) -> Result<()> {
    let node = zenui_embedded_node::ensure_available()
        .context("embedded Node is unavailable — cannot run npm")?;

    let npm_path = locate_npm(&node.bin_dir).ok_or_else(|| {
        anyhow!(
            "npm not found alongside embedded node at {}",
            node.bin_dir.display()
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
        "hydrating Copilot bridge node_modules via npm {install_subcommand} --omit=dev"
    );
    let started = std::time::Instant::now();

    let mut cmd = Command::new(&npm_path);
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
        .env("PATH", prepend_path(&node.bin_dir));

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
        "Copilot bridge node_modules hydrated"
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
