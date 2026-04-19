//! GitHub Copilot SDK bridge: embedded JS + node_modules tree.
//!
//! See `../provider-claude-sdk/src/bridge_runtime.rs` for the shared
//! extraction pattern; this module is a near-copy parameterised for
//! the copilot bridge paths. If you touch one, mirror the fix in the
//! other.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$OUT_DIR/bridge-assets/"]
struct BridgeAssets;

#[derive(Debug, Clone)]
pub struct BridgeRuntime {
    pub dir: PathBuf,
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
        anyhow::bail!("Copilot bridge assets are empty; rebuild with bridge/dist/index.js present");
    }

    let cache_root = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join(format!("copilot-bridge-{}", assets_fingerprint()));
    let script = cache_root.join("index.js");

    if script.exists() {
        return Ok(BridgeRuntime {
            dir: cache_root,
            script,
        });
    }

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
            "extracted Copilot bridge at {} is missing index.js",
            cache_root.display()
        );
    }

    Ok(BridgeRuntime {
        dir: cache_root,
        script,
    })
}

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
