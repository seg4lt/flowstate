//! Runtime HTTP download helper shared between `embedded-node` and the
//! provider-SDK bridge crates. Kept tiny on purpose — we only need a
//! "fetch a URL into a file" primitive that works from a blocking
//! context. Async callers bridge via `tokio::task::spawn_blocking`.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

/// Fetch `url` into `dest` atomically. Writes to `<dest>.partial` first
/// and renames on success so a crash mid-download never leaves a
/// truncated file that a later launch would mistake for a cache hit.
pub fn fetch(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let started = std::time::Instant::now();
    info!(%url, dest = %dest.display(), "downloading runtime asset");

    let response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;

    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body from {url}"))?;

    let tmp = dest.with_extension("partial");
    fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;

    info!(
        bytes = bytes.len(),
        duration_ms = started.elapsed().as_millis() as u64,
        dest = %dest.display(),
        "runtime asset downloaded"
    );
    Ok(())
}
