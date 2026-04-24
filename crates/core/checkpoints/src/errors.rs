//! Structured errors for the checkpoints subsystem.
//!
//! All public APIs in this crate return `Result<_, CheckpointError>`. Callers
//! pattern-match on the variant to decide whether the failure is recoverable
//! (e.g. `NoCheckpoint` → surface as `Unavailable` in the wire response) or
//! fatal (e.g. `Io` → log and skip capture for this turn).

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by `CheckpointStore` implementations.
///
/// `#[non_exhaustive]` so future variants are not a breaking change to
/// downstream consumers — pattern matches must include a wildcard arm
/// anyway because of the wide range of underlying failure modes.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckpointError {
    /// An `io::Error` at some filesystem boundary (walking the workspace,
    /// reading a file to hash, writing a blob, renaming a temp file). The
    /// `path` carries the target we were trying to act on when the error
    /// arose; `None` means the error is not tied to a single path (e.g. a
    /// broad directory walk that failed to list).
    #[error("io error at {path:?}: {source}")]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: io::Error,
    },

    /// A sqlite error from the checkpoints index or the file_state cache.
    #[error("sqlite error: {0}")]
    Sqlite(String),

    /// Manifest on disk could not be parsed — either its JSON is corrupt
    /// or its `version` field is newer than this daemon knows how to read.
    /// Data recovery is not attempted; the manifest is effectively
    /// unreadable and rewind to that checkpoint will surface as
    /// `NoCheckpoint` to the caller.
    #[error("manifest at {path:?} is corrupt or unknown version: {reason}")]
    ManifestCorrupt { path: PathBuf, reason: String },

    /// A blob referenced by a manifest is missing from the blob store.
    /// Usually indicates GC ran too eagerly (a bug) or manual tampering
    /// with `<data_dir>/checkpoints/blobs/`. Restore cannot produce the
    /// requested state without the blob.
    #[error("blob {hash} referenced by manifest is missing from the store")]
    BlobMissing { hash: String },

    /// No checkpoint exists for the requested `(session_id, turn_id)` pair.
    /// Most commonly: the session predates the feature, the capture failed
    /// silently, or the caller is using a stale turn id.
    #[error("no checkpoint found for session {session_id} turn {turn_id}")]
    NoCheckpoint { session_id: String, turn_id: String },

    /// A blake3 hash string did not parse as `blake3:<64 hex chars>`. Only
    /// raised by the `BlobHash` newtype constructor; the capture path
    /// produces canonical strings so this error is effectively a corrupted
    /// or externally-modified manifest.
    #[error("invalid blob hash format: {0}")]
    InvalidHash(String),

    /// The `root` passed to `capture` or `restore` is not a directory, or
    /// canonicalization failed.
    #[error("workspace root {root:?} is not a usable directory: {reason}")]
    InvalidRoot { root: PathBuf, reason: String },
}

impl From<io::Error> for CheckpointError {
    fn from(source: io::Error) -> Self {
        Self::Io { path: None, source }
    }
}

/// Construct an `Io` error and attach the path that triggered it. Used at
/// every explicit filesystem call-site so error messages can name the file
/// the user ultimately cares about.
///
/// Currently unused in PR 1 (the lib defines only the trait + types). Will
/// be called from `blob_store`, `capture`, `restore`, and `gc` in PR 2.
#[allow(dead_code)]
pub(crate) fn io_err(path: impl Into<PathBuf>, source: io::Error) -> CheckpointError {
    CheckpointError::Io {
        path: Some(path.into()),
        source,
    }
}
