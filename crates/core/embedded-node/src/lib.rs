//! Self-contained Node.js runtime used by the provider SDK bridges.
//!
//! Two modes, selected at compile time by the `embed` Cargo feature:
//!
//! - **`embed` ON** — `build.rs` downloads the official Node.js tarball
//!   (Unix) or zip (Windows) for the build target and this crate
//!   `include_bytes!`-es it into the compiled binary. At runtime
//!   [`ensure_available`] unpacks the embedded bytes into a per-user
//!   cache directory. No network needed after install. Self-contained /
//!   air-gapped friendly.
//!
//! - **`embed` OFF (default)** — the binary carries no Node.js payload.
//!   On first launch [`ensure_available`] downloads the same official
//!   tarball from `nodejs.org` into the same per-user cache directory
//!   and unpacks it in place. Subsequent launches hit the sentinel
//!   check and are a no-op.
//!
//! In both modes the on-disk layout is identical:
//! `~/.cache/zenui/embedded-node-v<version>/{bin/node,lib,...}`, so the
//! downstream provider crates and the spawn code are oblivious to which
//! mode built the binary.

pub mod download;
pub mod node_checksums;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use tracing::info;

/// Version of Node.js this crate targets. Must match the version
/// embedded by [`build.rs`] (embed mode) and the version downloaded at
/// runtime (download mode) — used below to namespace the cache
/// directory so bumping the version automatically invalidates any
/// stale extraction.
pub const NODE_VERSION: &str = "24.15.0";

/// Build-time platform identifiers forwarded from `build.rs` via
/// `cargo:rustc-env`. Empty strings when the build target is
/// unsupported; the runtime check below turns that into a clear error
/// instead of a silent mis-download.
const NODE_TARGET_PLATFORM: &str = env!("NODE_TARGET_PLATFORM");
#[cfg(not(feature = "embed"))]
const NODE_TARGET_ARCH: &str = env!("NODE_TARGET_ARCH");
#[cfg(not(feature = "embed"))]
const NODE_TARGET_EXT: &str = env!("NODE_TARGET_EXT");

/// The raw Node.js archive bytes, embedded at compile time. Only
/// populated when the `embed` feature is on.
///
/// Unix targets embed a `.tar.gz`, Windows embeds a `.zip`. Both files
/// are always present in OUT_DIR (the unused one is an empty stub) so
/// `include_bytes!` resolves on every platform.
#[cfg(all(feature = "embed", not(windows)))]
const NODE_ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/node.tar.gz"));
#[cfg(all(feature = "embed", windows))]
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

/// Ensure the Node.js runtime is available on disk and return its
/// paths. First call per process either extracts the embedded bytes
/// (embed mode) or downloads+extracts (default). Idempotent and safe
/// from multiple threads — the work runs once, subsequent callers
/// receive a cached result.
pub fn ensure_available() -> Result<NodeRuntime> {
    // `OnceLock` gives us "run once per process". If a previous call
    // returned an error we clone and re-raise rather than silently
    // retrying, because the error is almost always a deterministic
    // filesystem, network, or unsupported-target problem.
    let cached = EXTRACTED.get_or_init(|| provision_once().map_err(|e| format!("{e:?}")));
    cached.clone().map_err(|e| anyhow!(e))
}

/// Back-compat alias — existing call sites use `ensure_extracted()`.
/// Now just dispatches into [`ensure_available`], which covers both
/// the embed and download paths.
pub fn ensure_extracted() -> Result<NodeRuntime> {
    ensure_available()
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

fn provision_once() -> Result<NodeRuntime> {
    if NODE_TARGET_PLATFORM.is_empty() {
        anyhow::bail!(
            "zenui-embedded-node was built on an unsupported target; \
             no Node.js archive is embedded and no download URL is known"
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
        "provisioning embedded Node.js v{NODE_VERSION} to {}",
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

    let archive_bytes = load_archive_bytes()?;
    unpack_archive(&archive_bytes, &staging)?;

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

/// Source the raw archive bytes. With `embed` on, read straight from
/// the baked-in constant. Without `embed`, download to a persistent
/// cache under `~/.cache/zenui/node-downloads/` (shared with build.rs)
/// and read from there — so a failed extraction doesn't require a
/// second network round-trip.
fn load_archive_bytes() -> Result<Vec<u8>> {
    #[cfg(feature = "embed")]
    {
        if NODE_ARCHIVE.is_empty() {
            anyhow::bail!(
                "zenui-embedded-node was built with `embed` on an unsupported target; \
                 the embedded Node.js archive is empty"
            );
        }
        return Ok(NODE_ARCHIVE.to_vec());
    }

    #[cfg(not(feature = "embed"))]
    {
        let archive_path = download_cache_path()?;
        let expected_sha =
            node_checksums::expected_sha(NODE_TARGET_PLATFORM, NODE_TARGET_ARCH, NODE_TARGET_EXT)
                .ok_or_else(|| {
                anyhow!(
                    "no pinned SHA-256 in src/node_checksums.rs for \
                 {NODE_TARGET_PLATFORM}-{NODE_TARGET_ARCH}.{NODE_TARGET_EXT} \
                 (Node.js v{NODE_VERSION}); refusing to download an unverifiable archive"
                )
            })?;

        if !archive_path.exists() {
            let url = format!(
                "https://nodejs.org/dist/v{NODE_VERSION}/node-v{NODE_VERSION}-{platform}-{arch}.{ext}",
                platform = NODE_TARGET_PLATFORM,
                arch = NODE_TARGET_ARCH,
                ext = NODE_TARGET_EXT,
            );
            download::fetch_verified(&url, &archive_path, expected_sha)
                .with_context(|| format!("download Node.js v{NODE_VERSION} from {url}"))?;
        }

        // Re-verify the cached file every read. Cheap (~50 ms for 30 MB
        // on modern CPUs) and protects against a corrupted-on-disk
        // cache, a downgrade attempt that swapped the file behind our
        // back, or a stale archive left over from a previous
        // NODE_VERSION whose name happened to collide.
        let bytes = fs::read(&archive_path)
            .with_context(|| format!("read cached Node.js archive {}", archive_path.display()))?;
        download::verify_sha256(&bytes, expected_sha).with_context(|| {
            format!(
                "cached Node.js archive at {} failed SHA-256 verification — \
                 delete it and re-launch to re-download",
                archive_path.display()
            )
        })?;
        Ok(bytes)
    }
}

/// Persistent cache path for the downloaded Node.js archive. Shared
/// with `build.rs` (which writes to the same location when `embed` is
/// on) so a dev who flips the feature on after a download doesn't
/// re-fetch.
#[cfg(not(feature = "embed"))]
fn download_cache_path() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("failed to resolve per-user cache directory")?
        .join("zenui")
        .join("node-downloads");
    let filename = format!(
        "node-v{NODE_VERSION}-{platform}-{arch}.{ext}",
        platform = NODE_TARGET_PLATFORM,
        arch = NODE_TARGET_ARCH,
        ext = NODE_TARGET_EXT,
    );
    Ok(dir.join(filename))
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
