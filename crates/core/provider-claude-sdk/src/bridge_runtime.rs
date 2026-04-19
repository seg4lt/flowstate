//! Claude SDK bridge: embedded JS + node_modules tree.
//!
//! At build time `build.rs` stages `index.js`, `package.json`, and the
//! entire `node_modules/` directory into `$OUT_DIR/bridge-assets/`.
//! rust-embed walks that tree and embeds every file as a static
//! [`u8]` blob inside the compiled crate. On first call to
//! [`ensure_extracted`] we write those files into a per-user cache
//! directory and return the paths the spawn code needs.
//!
//! Subsequent calls in the same process are a `OnceLock` hit; across
//! processes, they hit the cache-dir sentinel (presence of `index.js`).

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use rust_embed::Embed;

/// Every file staged into `$OUT_DIR/bridge-assets/` by `build.rs`,
/// embedded as static bytes in the compiled crate.
#[derive(Embed)]
#[folder = "$OUT_DIR/bridge-assets/"]
struct BridgeAssets;

/// Fingerprint of `bridge/dist/index.js` written by `build.rs`. The
/// `include_str!` is load-bearing: rustc tracks it in dep-info, so when
/// `build.rs` rewrites this file (content hash of dist changed),
/// rustc recompiles this module and the `#[derive(Embed)]` proc macro
/// above re-scans `$OUT_DIR/bridge-assets/` with the fresh bytes.
/// Without this reference, the previous build's bridge stays embedded
/// in the binary even after a fresh `bun run build` updates the on-disk
/// assets, and the daemon ends up spawning a stale bridge at runtime.
#[allow(dead_code)]
const BRIDGE_ASSETS_FINGERPRINT: &str =
    include_str!(concat!(env!("OUT_DIR"), "/bridge-assets-fingerprint.txt"));

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

pub fn ensure_extracted() -> Result<BridgeRuntime> {
    let cached = EXTRACTED.get_or_init(|| extract_once().map_err(|e| format!("{e:?}")));
    cached.clone().map_err(|e| anyhow!(e))
}

fn extract_once() -> Result<BridgeRuntime> {
    let file_count = BridgeAssets::iter().count();
    if file_count == 0 {
        anyhow::bail!(
            "Claude SDK bridge assets are empty; rebuild with bridge/dist/index.js present"
        );
    }

    let cache_root = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join(format!("claude-sdk-bridge-{}", assets_fingerprint()));
    let script = cache_root.join("index.js");

    if script.exists() {
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

    // Atomic extraction via a staging sibling directory, renamed at the
    // end. This avoids leaving a half-populated cache if the process
    // dies mid-extract.
    let staging = cache_root.with_extension("extracting");
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;

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
        // rust-embed strips Unix file mode, so bundled binaries (e.g. the
        // ripgrep shipped inside claude-agent-sdk) come out non-executable
        // and spawning them fails with EACCES. Restore +x on vendored bins.
        #[cfg(unix)]
        if is_vendored_binary(&path) {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target)
                .with_context(|| format!("stat {}", target.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&target, perms)
                .with_context(|| format!("chmod +x {}", target.display()))?;
        }
    }

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

    Ok(BridgeRuntime {
        dir: cache_root,
        script,
    })
}

/// True for files that ship as native executables inside `node_modules`
/// and therefore need the execute bit restored after extraction. Today
/// the only case is the ripgrep binary bundled with
/// `@anthropic-ai/claude-agent-sdk` under `vendor/ripgrep/<target>/rg`.
fn is_vendored_binary(path: &str) -> bool {
    path.contains("/vendor/ripgrep/")
        && matches!(path.rsplit('/').next(), Some("rg") | Some("rg.exe"))
}

/// Deterministic fingerprint of the embedded bridge assets. Used as a
/// cache-dir namespace so any change to a bridge file (rebuild) goes
/// to a fresh cache directory instead of overwriting old files
/// in-place. Inline FNV-1a — no dep, deterministic, ~1 µs on a ~10k
/// file tree.
fn assets_fingerprint() -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut parts: Vec<(String, usize)> = BridgeAssets::iter()
        .map(|p| {
            let size = BridgeAssets::get(&p).map(|f| f.data.len()).unwrap_or(0);
            (p.to_string(), size)
        })
        .collect();
    parts.sort();
    for (name, size) in &parts {
        for b in name.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        for b in size.to_le_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}
