//! Content-addressed blob store.
//!
//! Layout: `<root>/<hh>/<rest-of-hash>` where `hh` is the first two hex
//! chars of the blake3 digest and `rest-of-hash` is the remaining 62
//! chars. Two-level layout keeps per-directory counts bounded (≤ 256
//! top-level entries, ≤ ~64k blobs under each before any filesystem
//! starts complaining).
//!
//! All writes are atomic: temp-file-then-rename. A crash between the
//! write and the rename leaves a `.tmp` orphan that the GC sweep picks
//! up — never a partially-written blob masquerading as valid content.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::errors::{CheckpointError, io_err};
use crate::manifest::BlobHash;

/// Thin wrapper around a directory where blobs live. Stateless by
/// design — the filesystem IS the state.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, CheckpointError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io_err(root.clone(), e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute the absolute path a blob with the given hash would live
    /// at (whether or not it actually exists).
    pub fn path_for(&self, hash: &BlobHash) -> PathBuf {
        let hex = hash.hex();
        // hex is guaranteed to be 64 chars by `BlobHash::from_hex`.
        let (head, tail) = hex.split_at(2);
        self.root.join(head).join(tail)
    }

    pub fn exists(&self, hash: &BlobHash) -> bool {
        self.path_for(hash).is_file()
    }

    /// Write `bytes` under its blake3 hash and return the hash. If the
    /// blob already exists (same content), this is a no-op. Writes are
    /// atomic: temp + rename so observers never see a half-blob.
    pub fn write_if_absent(&self, bytes: &[u8]) -> Result<BlobHash, CheckpointError> {
        let hash = BlobHash::hash_bytes(bytes);
        let final_path = self.path_for(&hash);
        if final_path.is_file() {
            return Ok(hash);
        }
        let parent = final_path.parent().expect("blob path always has a parent");
        fs::create_dir_all(parent).map_err(|e| io_err(parent.to_path_buf(), e))?;

        // Temp file in the same parent dir so the rename is guaranteed
        // atomic (no cross-device concern). Suffix with the hash so
        // parallel writes of different blobs don't collide, and prefix
        // with `.` so a mid-write crash leaves a hidden orphan that
        // doesn't pollute directory listings.
        let tmp_path = parent.join(format!(".{}.tmp", hash.hex()));
        fs::write(&tmp_path, bytes).map_err(|e| io_err(tmp_path.clone(), e))?;
        // Another thread or process might have written the same blob
        // while we were building ours — in which case `rename` onto an
        // existing file will succeed on POSIX (atomic replace) but
        // either way the end state is correct: the blob with this hash
        // is on disk. We prefer to keep the original.
        if final_path.is_file() {
            // Ours is redundant. Best-effort cleanup of the temp.
            let _ = fs::remove_file(&tmp_path);
        } else {
            fs::rename(&tmp_path, &final_path).map_err(|e| io_err(final_path.clone(), e))?;
        }
        Ok(hash)
    }

    /// Read a blob's full contents. Errors if the blob is missing.
    pub fn read(&self, hash: &BlobHash) -> Result<Vec<u8>, CheckpointError> {
        let path = self.path_for(hash);
        if !path.is_file() {
            return Err(CheckpointError::BlobMissing {
                hash: hash.as_str().to_string(),
            });
        }
        let mut file = fs::File::open(&path).map_err(|e| io_err(path.clone(), e))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| io_err(path.clone(), e))?;
        Ok(buf)
    }

    /// Delete a blob by hash. Missing-file is treated as success — GC
    /// may call this concurrently with other ops and we don't want a
    /// race to surface as an error.
    pub fn delete(&self, hash: &BlobHash) -> Result<(), CheckpointError> {
        let path = self.path_for(hash);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(path, e)),
        }
    }

    /// Iterate over every blob currently on disk, yielding its hash.
    /// Used by GC to compare against the set of hashes still referenced.
    pub fn iter_hashes(&self) -> Result<Vec<BlobHash>, CheckpointError> {
        let mut out = Vec::new();
        let top = match fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(io_err(self.root.clone(), e)),
        };
        for top_entry in top.flatten() {
            let head_name = match top_entry.file_name().into_string() {
                Ok(s) if s.len() == 2 && s.chars().all(|c| c.is_ascii_hexdigit()) => s,
                _ => continue, // skip stray files at the top level
            };
            let head_path = top_entry.path();
            let inner = match fs::read_dir(&head_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for inner_entry in inner.flatten() {
                let tail_name = match inner_entry.file_name().into_string() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // Skip .tmp orphans — GC cleans those via a separate
                // pass since they're scoped to parent directories.
                if tail_name.starts_with('.') {
                    continue;
                }
                if tail_name.len() != 62 || !tail_name.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let full_hex = format!("{head_name}{tail_name}");
                if let Ok(h) = BlobHash::from_hex(&full_hex) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    /// Sweep temp-file orphans (.tmp) left behind by a mid-write crash.
    /// Called as part of GC.
    pub fn sweep_tmp_orphans(&self) -> Result<usize, CheckpointError> {
        let mut count = 0;
        let top = match fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(io_err(self.root.clone(), e)),
        };
        for top_entry in top.flatten() {
            let head_path = top_entry.path();
            if !head_path.is_dir() {
                continue;
            }
            let inner = match fs::read_dir(&head_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for inner_entry in inner.flatten() {
                let name = inner_entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if name_str.starts_with('.') && name_str.ends_with(".tmp") {
                    let _ = fs::remove_file(inner_entry.path());
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_if_absent_round_trips() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let hash = store.write_if_absent(b"hello world").unwrap();
        assert!(store.exists(&hash));
        let got = store.read(&hash).unwrap();
        assert_eq!(got, b"hello world");
    }

    #[test]
    fn write_if_absent_is_content_addressed() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let h1 = store.write_if_absent(b"same").unwrap();
        let h2 = store.write_if_absent(b"same").unwrap();
        assert_eq!(h1, h2);
        // Count entries in the two-level layout to confirm dedup.
        let all = store.iter_hashes().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn path_for_uses_two_level_split() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let hash = BlobHash::from_hex(&"a".repeat(64)).unwrap();
        let p = store.path_for(&hash);
        let rel = p.strip_prefix(dir.path()).unwrap();
        assert_eq!(rel, Path::new(&format!("aa/{}", "a".repeat(62))));
    }

    #[test]
    fn delete_is_idempotent_for_missing_blobs() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let hash = BlobHash::from_hex(&"b".repeat(64)).unwrap();
        // Never existed — delete should still succeed.
        store.delete(&hash).unwrap();
    }

    #[test]
    fn read_missing_blob_is_structured_error() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let hash = BlobHash::from_hex(&"c".repeat(64)).unwrap();
        match store.read(&hash) {
            Err(CheckpointError::BlobMissing { .. }) => {}
            other => panic!("expected BlobMissing, got {other:?}"),
        }
    }

    #[test]
    fn iter_hashes_roundtrips_written_blobs() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let h1 = store.write_if_absent(b"one").unwrap();
        let h2 = store.write_if_absent(b"two").unwrap();
        let mut found = store.iter_hashes().unwrap();
        found.sort_by_key(|h| h.as_str().to_string());
        let mut expected = vec![h1, h2];
        expected.sort_by_key(|h| h.as_str().to_string());
        assert_eq!(found, expected);
    }

    #[test]
    fn sweep_tmp_orphans_removes_dot_tmp_files() {
        let dir = TempDir::new().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        // Simulate a crash-left orphan.
        let head = dir.path().join("ab");
        fs::create_dir_all(&head).unwrap();
        let orphan = head.join(".crashed.tmp");
        fs::write(&orphan, b"partial").unwrap();
        let removed = store.sweep_tmp_orphans().unwrap();
        assert_eq!(removed, 1);
        assert!(!orphan.exists());
    }
}
