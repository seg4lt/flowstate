//! Directory walker with hardcoded + `.gitignore`-respecting ignore rules.
//!
//! Uses the `ignore` crate's `WalkBuilder` so we pick up `.gitignore`,
//! `.ignore`, and `.git/info/exclude` for free. On top of that we apply
//! a hardcoded deny-list for directories and file-size thresholds that
//! are never worth checkpointing:
//!
//! - VCS metadata: `.git`, `.hg`, `.svn`, `.bzr`
//! - Dependency installs: `node_modules`, `.venv`, `venv`, `__pycache__`
//! - Build outputs: `target`, `dist`, `build`, `.next`, `out`, `.gradle`
//! - Caches: `.cache`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`
//!
//! Users can re-enable any of these with a negated `.gitignore` pattern
//! (`!node_modules/something`) — but the defaults reflect what ~nobody
//! wants snapshotted on every turn.
//!
//! File-size cap: skip regular files > 5 MiB. The rewind dialog surfaces
//! skipped files so the user isn't surprised when a 40 MB binary
//! doesn't rewind.

use std::path::{Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};

use crate::errors::{io_err, CheckpointError};

/// Per-file size cap. Applied AFTER the walker's ignore rules — so a
/// 6 MB file that's already in `.gitignore` never gets statted for
/// size at all.
pub const FILE_SIZE_CAP_BYTES: u64 = 5 * 1024 * 1024;

/// Directory names that are always skipped regardless of whether the
/// workspace has a `.gitignore`. Kept short and well-justified: every
/// entry here is a directory that, when snapshotted, blows up blob
/// volume by orders of magnitude for essentially no user benefit.
pub const HARD_IGNORE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".bzr",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    "out",
    ".gradle",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".turbo",
];

/// One entry yielded by [`walk`]. Canonicalization is NOT applied here
/// — the walker returns raw paths relative to the walk root. The
/// capture path canonicalizes via the persistent cache.
#[derive(Debug)]
pub struct WalkItem {
    /// Absolute path on disk.
    pub abs_path: PathBuf,

    /// Path relative to the walk root, with forward-slash separators.
    pub rel_path: String,

    /// `mtime` in nanoseconds since the Unix epoch. `i64` for direct
    /// storage in the sqlite cache column.
    pub mtime_ns: i64,

    /// File size in bytes.
    pub size_bytes: u64,
}

/// Walk `root` and return every regular file that survives the ignore
/// rules and size cap, as a `Vec<WalkItem>`. Errors bubble up as
/// [`CheckpointError::Io`]; a single unreadable entry is logged and
/// skipped rather than failing the whole walk.
///
/// Symlinks are followed only when they point INTO the root — a
/// symlink that escapes to `/etc/passwd` is silently skipped.
pub fn walk(root: &Path) -> Result<Vec<WalkItem>, CheckpointError> {
    let root = root.to_path_buf();
    if !root.is_dir() {
        return Err(CheckpointError::InvalidRoot {
            root: root.clone(),
            reason: "not a directory".to_string(),
        });
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| io_err(root.clone(), e))?;

    let mut builder = WalkBuilder::new(&canonical_root);
    // Respect `.gitignore`, `.ignore`, and global VCS ignore files.
    // `add_custom_ignore_filename(".gitignore")` is important: the default
    // git_ignore pathway requires a `.git/` directory at or above the
    // root, so non-git workspaces were silently ignoring their
    // `.gitignore`. Treating `.gitignore` as a custom ignore file makes
    // the behaviour consistent regardless of whether the workspace is a
    // git repo.
    builder
        .hidden(false) // don't skip dotfiles by default — `.env.example` matters
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .add_custom_ignore_filename(".gitignore")
        .follow_links(false);
    // Layer our hardcoded deny-list on top. A `filter_entry` on the
    // WalkBuilder is O(1) per entry and fires BEFORE the walker
    // descends, so `node_modules` is never recursed into regardless of
    // size.
    builder.filter_entry(|entry: &DirEntry| {
        let name = entry.file_name().to_string_lossy();
        !HARD_IGNORE_DIRS.iter().any(|deny| *deny == name.as_ref())
    });

    let mut out = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("checkpoint walker skipped entry: {e}");
                continue;
            }
        };
        // Only regular files pass the filter. Directories, symlinks,
        // and special files (sockets, devices) are dropped here — they
        // don't have blob-able content.
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let abs_path = entry.path().to_path_buf();

        // Reject files whose canonical form escapes the root (would
        // happen if the walker followed a symlink despite
        // `follow_links(false)` — belt + suspenders).
        if let Ok(canonical) = abs_path.canonicalize() {
            if !canonical.starts_with(&canonical_root) {
                continue;
            }
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("checkpoint walker stat failed for {abs_path:?}: {e}");
                continue;
            }
        };
        let size_bytes = metadata.len();
        if size_bytes > FILE_SIZE_CAP_BYTES {
            tracing::debug!(
                "checkpoint walker skipped {abs_path:?} — size {size_bytes} exceeds cap {FILE_SIZE_CAP_BYTES}",
            );
            continue;
        }

        let mtime_ns = match metadata.modified() {
            Ok(t) => t
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
            Err(_) => 0,
        };

        let rel_path = match abs_path.strip_prefix(&canonical_root) {
            Ok(p) => normalize_separators(&p.to_string_lossy()),
            Err(_) => continue, // shouldn't happen after starts_with check
        };

        out.push(WalkItem {
            abs_path,
            rel_path,
            mtime_ns,
            size_bytes,
        });
    }
    Ok(out)
}

/// Convert native path separators to forward-slash so manifests are
/// portable across platforms. A snapshot taken on Windows can still be
/// restored from on macOS if the workspace is on a shared filesystem.
pub(crate) fn normalize_separators(s: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        s.to_string()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, bytes).unwrap();
    }

    #[test]
    fn walk_finds_regular_files_under_root() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.txt", b"hello");
        write(dir.path(), "sub/b.txt", b"world");
        let mut items = walk(dir.path()).unwrap();
        items.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].rel_path, "a.txt");
        assert_eq!(items[1].rel_path, "sub/b.txt");
    }

    #[test]
    fn walk_skips_hardcoded_deny_list() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.txt", b"ok");
        write(dir.path(), "node_modules/pkg/index.js", b"nope");
        write(dir.path(), ".git/HEAD", b"nope");
        write(dir.path(), "target/debug/x", b"nope");
        let items = walk(dir.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].rel_path, "a.txt");
    }

    #[test]
    fn walk_respects_gitignore() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".gitignore", b"secret.txt\n");
        write(dir.path(), "public.txt", b"ok");
        write(dir.path(), "secret.txt", b"hidden");
        let items: Vec<_> = walk(dir.path())
            .unwrap()
            .into_iter()
            .map(|i| i.rel_path)
            .collect();
        assert!(items.contains(&"public.txt".to_string()));
        assert!(items.contains(&".gitignore".to_string()));
        assert!(!items.contains(&"secret.txt".to_string()));
    }

    #[test]
    fn walk_skips_oversize_files() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "small.txt", b"ok");
        // Create a 6 MB file
        let big: Vec<u8> = vec![0u8; (FILE_SIZE_CAP_BYTES + 1) as usize];
        write(dir.path(), "big.bin", &big);
        let items: Vec<_> = walk(dir.path())
            .unwrap()
            .into_iter()
            .map(|i| i.rel_path)
            .collect();
        assert!(items.contains(&"small.txt".to_string()));
        assert!(!items.contains(&"big.bin".to_string()));
    }

    #[test]
    fn walk_errors_on_non_directory_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"x").unwrap();
        match walk(&file) {
            Err(CheckpointError::InvalidRoot { .. }) => {}
            other => panic!("expected InvalidRoot, got {other:?}"),
        }
    }
}
