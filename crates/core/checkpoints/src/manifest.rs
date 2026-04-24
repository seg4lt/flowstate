//! Manifest format — one per turn checkpoint.
//!
//! A manifest is the list of files this session touched during the turn, each
//! entry carrying the blake3 hash of the file's content BEFORE the turn
//! (`pre_hash`) and AFTER the turn (`post_hash`). Restoration algorithm uses
//! those hashes to locate blobs in the content-addressed store.
//!
//! Manifests are serialized as JSON. Atomic temp-then-rename writes guarantee
//! that a partial file never survives a crash.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::errors::{CheckpointError, io_err};

/// On-disk format version. Bump when changing the manifest shape in a way
/// that older daemons can't read. Readers must reject unknown versions
/// loudly rather than silently accepting garbage.
pub const MANIFEST_VERSION: u32 = 1;

/// One manifest describes one checkpoint for one turn. Stored as
/// `<data_dir>/checkpoints/manifests/<checkpoint_id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Schema version. Must equal `MANIFEST_VERSION` on read.
    pub version: u32,

    /// Unique id for this checkpoint. Also the filename stem.
    pub checkpoint_id: String,

    pub session_id: String,
    pub turn_id: String,

    /// RFC 3339 timestamp when capture completed.
    pub created_at: String,

    /// Absolute canonical path of the workspace root at capture time.
    /// Stored so a rewind can sanity-check the root the caller is asking
    /// us to restore to.
    pub root: String,

    /// Files this session touched during the turn this manifest covers.
    pub touched: Vec<ManifestEntry>,
}

/// One row of a `Manifest`. `pre_hash: None` ⇔ file did not exist before
/// the turn (it was created during the turn). `post_hash: None` ⇔ file no
/// longer exists (it was deleted during the turn).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    /// Path relative to `Manifest.root`. Forward-slash normalized on all
    /// platforms so manifests are portable if a project directory is
    /// moved between machines with matching layouts.
    pub path: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_hash: Option<BlobHash>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_hash: Option<BlobHash>,
}

/// A newtype wrapping a canonical blake3-hash string. Format:
/// `blake3:<64 lowercase hex chars>`. Constructed via `BlobHash::new` or
/// via serde deserialization, both of which validate the shape.
///
/// Using a newtype keeps the distinction between "any string" and "a valid
/// hash" visible in signatures — so callers can't accidentally pass a raw
/// file path where a hash is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobHash(String);

impl BlobHash {
    /// Build from a raw blake3 hex digest (64 chars, no prefix). Prepends
    /// the `blake3:` scheme and returns a validated newtype.
    pub fn from_hex(hex: &str) -> Result<Self, CheckpointError> {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CheckpointError::InvalidHash(hex.to_string()));
        }
        // Normalize to lowercase so the blob-store path lookup is
        // deterministic regardless of how the caller spelled the hex.
        Ok(Self(format!("blake3:{}", hex.to_lowercase())))
    }

    /// Hash arbitrary bytes with blake3 and wrap the hex digest.
    pub fn hash_bytes(bytes: &[u8]) -> Self {
        let digest = blake3::hash(bytes);
        // blake3 hex output is lowercase already; safe to wrap without
        // going through `from_hex`'s validation loop.
        Self(format!("blake3:{}", digest.to_hex()))
    }

    /// Parse an already-prefixed hash string (`blake3:<hex>`). Used on the
    /// deserialization path.
    pub fn parse(raw: &str) -> Result<Self, CheckpointError> {
        let Some(rest) = raw.strip_prefix("blake3:") else {
            return Err(CheckpointError::InvalidHash(raw.to_string()));
        };
        Self::from_hex(rest)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 64-char hex digest without the scheme prefix. Used when
    /// computing the two-level blob-store path (`<hh>/<rest>`).
    pub fn hex(&self) -> &str {
        // Safe: constructor ensures the prefix length is exactly `blake3:` (7).
        &self.0[7..]
    }
}

impl std::fmt::Display for BlobHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for BlobHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BlobHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        BlobHash::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Manifest {
    /// Read a manifest from disk and validate its `version` field
    /// against [`MANIFEST_VERSION`]. Unknown versions surface as
    /// [`CheckpointError::ManifestCorrupt`] so callers treat them as
    /// "this manifest is unreadable" rather than silently accepting
    /// partial data.
    pub fn load(path: &Path) -> Result<Self, CheckpointError> {
        let bytes = std::fs::read(path).map_err(|e| io_err(path.to_path_buf(), e))?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| CheckpointError::ManifestCorrupt {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        if manifest.version != MANIFEST_VERSION {
            return Err(CheckpointError::ManifestCorrupt {
                path: path.to_path_buf(),
                reason: format!(
                    "unsupported manifest version {} (expected {MANIFEST_VERSION})",
                    manifest.version
                ),
            });
        }
        Ok(manifest)
    }

    /// Serialize `self` and write atomically: write to a temp file in
    /// the same directory, then `rename()` onto `path`. Callers can
    /// rely on the file either being absent or fully-formed — never
    /// partially written — even across a daemon crash.
    pub fn write_atomic(&self, path: &Path) -> Result<(), CheckpointError> {
        let parent = path.parent().ok_or_else(|| CheckpointError::InvalidRoot {
            root: path.to_path_buf(),
            reason: "manifest path has no parent directory".to_string(),
        })?;
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent.to_path_buf(), e))?;
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|e| CheckpointError::ManifestCorrupt {
                path: path.to_path_buf(),
                reason: format!("serialize manifest: {e}"),
            })?;
        let tmp_name = format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("manifest")
        );
        let tmp_path = parent.join(tmp_name);
        std::fs::write(&tmp_path, &bytes).map_err(|e| io_err(tmp_path.clone(), e))?;
        std::fs::rename(&tmp_path, path).map_err(|e| io_err(path.to_path_buf(), e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_hash_from_hex_validates_length() {
        assert!(BlobHash::from_hex("aa").is_err());
        let h = BlobHash::from_hex(&"a".repeat(64)).unwrap();
        assert_eq!(h.as_str(), format!("blake3:{}", "a".repeat(64)));
    }

    #[test]
    fn blob_hash_from_hex_rejects_non_hex() {
        let mut s = "a".repeat(63);
        s.push('g'); // 64 chars but 'g' isn't hex
        assert!(BlobHash::from_hex(&s).is_err());
    }

    #[test]
    fn blob_hash_normalizes_to_lowercase() {
        let upper = "A".repeat(64);
        let h = BlobHash::from_hex(&upper).unwrap();
        assert_eq!(h.hex(), "a".repeat(64).as_str());
    }

    #[test]
    fn blob_hash_parse_round_trips_json() {
        let h = BlobHash::from_hex(&"b".repeat(64)).unwrap();
        let j = serde_json::to_string(&h).unwrap();
        let back: BlobHash = serde_json::from_str(&j).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn blob_hash_parse_rejects_missing_prefix() {
        assert!(BlobHash::parse(&"c".repeat(64)).is_err());
    }

    #[test]
    fn blob_hash_hex_slices_past_prefix() {
        let h = BlobHash::from_hex(&"d".repeat(64)).unwrap();
        assert_eq!(h.hex().len(), 64);
    }

    #[test]
    fn manifest_serializes_null_hashes_compactly() {
        let m = Manifest {
            version: MANIFEST_VERSION,
            checkpoint_id: "cp_1".into(),
            session_id: "s1".into(),
            turn_id: "t1".into(),
            created_at: "2026-04-19T00:00:00Z".into(),
            root: "/tmp/x".into(),
            touched: vec![ManifestEntry {
                path: "a.rs".into(),
                pre_hash: None,
                post_hash: Some(BlobHash::from_hex(&"e".repeat(64)).unwrap()),
            }],
        };
        let j = serde_json::to_string(&m).unwrap();
        // `pre_hash: null` should be omitted by skip_serializing_if.
        assert!(!j.contains("preHash"));
        assert!(j.contains("postHash"));
    }
}
