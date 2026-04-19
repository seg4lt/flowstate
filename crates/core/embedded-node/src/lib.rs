//! Self-contained Node.js runtime embedded into the zenui binary.
//!
//! The build script downloads the official Node.js tarball (Unix) or zip
//! (Windows) for the build target and this crate `include_bytes!`-es it
//! into the compiled binary. At runtime, [`ensure_extracted`] lazily
//! unpacks the archive into a per-user cache directory (first call
//! extracts, subsequent calls are a fast sentinel check) and returns the
//! paths callers need to spawn the embedded `node` executable.
//!
//! The archive is compressed (~20 MB) and shared by all providers that
//! need a Node runtime, so embedding it once is much cheaper than each
//! provider carrying its own copy.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use tracing::info;

/// Version of Node.js embedded by [`build.rs`]. Must match the version
/// downloaded there — used below to namespace the cache directory so
/// bumping the version automatically invalidates any stale extraction.
pub const NODE_VERSION: &str = "20.11.1";

/// The raw Node.js archive bytes, embedded at compile time. Empty if
/// the build target is unsupported (in which case [`ensure_extracted`]
/// will return an error).
///
/// Unix targets embed a `.tar.gz`, Windows embeds a `.zip`. Both files
/// are always present in OUT_DIR (the unused one is an empty stub) so
/// `include_bytes!` resolves on every platform.
#[cfg(not(windows))]
const NODE_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node.tar.gz"));
#[cfg(windows)]
const NODE_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node.zip"));

/// Resolved paths inside an extracted Node.js runtime.
#[derive(Debug, Clone)]
pub struct NodeRuntime {
    /// Absolute path to the `node` executable.
    pub node_bin: PathBuf,
    /// Absolute path to the directory containing `node` — use this when
    /// building a child process `PATH` so Node's own subprocess spawns
    /// (e.g. worker threads, the Claude Agent SDK's internal `node`
    /// calls) resolve to the same embedded runtime.
    pub bin_dir: PathBuf,
}

static EXTRACTED: OnceLock<Result<NodeRuntime, String>> = OnceLock::new();

/// Ensure the embedded Node.js runtime has been unpacked into the
/// per-user cache directory and return its paths. Idempotent and safe
/// to call from multiple threads — the work runs once, subsequent
/// callers receive a cached result.
pub fn ensure_extracted() -> Result<NodeRuntime> {
    // `OnceLock` gives us "extract once per process". If a previous
    // call returned an error we clone and re-raise it rather than
    // silently retrying, because the error is almost always a
    // deterministic filesystem or unsupported-target problem.
    let cached = EXTRACTED.get_or_init(|| extract_once().map_err(|e| format!("{e:?}")));
    cached.clone().map_err(|e| anyhow!(e))
}

/// Platform-specific path to the `node` binary relative to the
/// extraction root. On Unix the tarball layout puts it at `bin/node`;
/// on Windows the zip puts `node.exe` at the root.
#[cfg(not(windows))]
fn node_bin_relative(root: &Path) -> PathBuf {
    root.join("bin").join("node")
}

#[cfg(windows)]
fn node_bin_relative(root: &Path) -> PathBuf {
    root.join("node.exe")
}

fn extract_once() -> Result<NodeRuntime> {
    if NODE_ARCHIVE.is_empty() {
        anyhow::bail!(
            "zenui-embedded-node was built on an unsupported target; no Node.js archive is embedded"
        );
    }

    let cache_root = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join(format!("embedded-node-v{NODE_VERSION}"));

    let node_bin = node_bin_relative(&cache_root);

    // Fast path: already extracted. We treat the presence of the node
    // binary as the only sentinel — if a previous extraction was
    // interrupted the directory will lack this file and we'll
    // re-extract.
    if node_bin.exists() {
        let bin_dir = node_bin
            .parent()
            .expect("node_bin always has a parent")
            .to_path_buf();
        return Ok(NodeRuntime { node_bin, bin_dir });
    }

    info!(
        "extracting embedded Node.js v{NODE_VERSION} to {}",
        cache_root.display()
    );

    // Extract into a sibling temp dir first, then atomically rename so
    // a crash mid-extract doesn't leave a half-populated cache that the
    // sentinel check above would incorrectly accept as valid.
    let staging = cache_root.with_extension("extracting");
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;

    unpack_archive(NODE_ARCHIVE, &staging)?;

    // The Node.js archive has a top-level `node-v<version>-<os>-<arch>/`
    // directory we need to strip so the final layout is
    // `embedded-node-v<version>/{bin,lib,...}` (Unix) or
    // `embedded-node-v<version>/{node.exe,...}` (Windows).
    let top = find_single_subdir(&staging)
        .context("expected exactly one top-level directory in Node.js archive")?;

    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)
            .with_context(|| format!("remove existing cache root {}", cache_root.display()))?;
    }
    if let Some(parent) = cache_root.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create cache parent {}", parent.display()))?;
    }
    fs::rename(&top, &cache_root)
        .with_context(|| format!("rename {} -> {}", top.display(), cache_root.display()))?;

    // Clean up the staging shell (now empty after the rename).
    fs::remove_dir_all(&staging).ok();

    if !node_bin.exists() {
        #[cfg(not(windows))]
        let expected = "bin/node";
        #[cfg(windows)]
        let expected = "node.exe";
        anyhow::bail!(
            "extracted Node.js runtime at {} is missing {expected}",
            cache_root.display()
        );
    }

    // tar preserves mode bits, so bin/node should already be executable.
    // Double-check on Unix — a misconfigured umask can strip exec bits
    // on some filesystems, and a non-executable `node` fails cryptically.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&node_bin)
            .with_context(|| format!("stat {}", node_bin.display()))?
            .permissions();
        if perms.mode() & 0o111 == 0 {
            perms.set_mode(perms.mode() | 0o755);
            fs::set_permissions(&node_bin, perms)
                .with_context(|| format!("chmod +x {}", node_bin.display()))?;
        }
    }

    info!(
        "embedded Node.js v{NODE_VERSION} ready at {}",
        node_bin.display()
    );

    let bin_dir = node_bin
        .parent()
        .expect("node_bin always has a parent")
        .to_path_buf();
    Ok(NodeRuntime { node_bin, bin_dir })
}

// ---------------------------------------------------------------------------
// Archive extraction — tar.gz on Unix, zip on Windows
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn unpack_archive(bytes: &[u8], dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(bytes);
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .with_context(|| format!("unpack Node.js tarball into {}", dest.display()))?;
    Ok(())
}

#[cfg(windows)]
fn unpack_archive(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::io::Cursor;

    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).context("failed to open Node.js zip archive")?;
    archive
        .extract(dest)
        .with_context(|| format!("unpack Node.js zip into {}", dest.display()))?;
    Ok(())
}

fn find_single_subdir(parent: &Path) -> Result<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for entry in fs::read_dir(parent).with_context(|| format!("read_dir {}", parent.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            if found.is_some() {
                anyhow::bail!(
                    "expected exactly one directory inside {}, found multiple",
                    parent.display()
                );
            }
            found = Some(entry.path());
        }
    }
    found.ok_or_else(|| anyhow!("no subdirectories found inside {}", parent.display()))
}
