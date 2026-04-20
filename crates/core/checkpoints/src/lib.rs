//! Provider-agnostic workspace checkpoints.
//!
//! A [`CheckpointStore`] captures and restores snapshots of a session's
//! workspace at turn boundaries. The default implementation
//! ([`FsCheckpointStore`], lands in PR 2) uses a content-addressed blob
//! store plus per-turn JSON manifests, keyed off a persistent
//! `(path, mtime, size) → blob_hash` cache so per-turn cost is O(files
//! touched), not O(workspace size).
//!
//! # Why this crate exists separately from `runtime-core`
//!
//! Checkpoints are a runtime concern but they're independently reasoned
//! about: they know nothing about provider adapters, turns-as-messages, or
//! the orchestration layer. Splitting the crate out lets us:
//!
//! - Test the snapshot machinery without spinning up a fake provider
//!   pipeline.
//! - Swap the default `FsCheckpointStore` for a `NoopCheckpointStore` in
//!   tests that don't care about rewind.
//! - Keep `runtime-core`'s dep graph from growing another rusqlite path
//!   once PR 2 wires the sqlite-backed index.
//!
//! # Provider agnosticism
//!
//! By design, this crate never mentions `ProviderAdapter`, `ProviderKind`,
//! or any provider-specific types. Captures operate on the filesystem at
//! `root` regardless of how files got there. This is the whole point —
//! bash edits, MCP edits, Claude's Write tool, Codex's apply_patch, future
//! providers we haven't built yet: all snapshottable, all restorable.

use std::path::Path;

use async_trait::async_trait;

pub mod errors;
pub mod manifest;

#[cfg(test)]
mod noop_tests;

pub use errors::CheckpointError;
pub use manifest::{BlobHash, Manifest, ManifestEntry, MANIFEST_VERSION};

/// Handle returned by a successful [`CheckpointStore::capture`]. Carries
/// enough metadata for the caller to publish a `CheckpointCaptured` event
/// without a second round-trip through the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointHandle {
    pub checkpoint_id: String,
    pub session_id: String,
    pub turn_id: String,
    /// RFC 3339 timestamp.
    pub created_at: String,
}

/// Options the caller passes to [`CheckpointStore::restore`]. Kept as a
/// struct (instead of positional booleans) so adding new options is not a
/// breaking API change — default behavior stays identical for callers that
/// don't set the new field.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// When `true`, the store computes the set of paths that WOULD change
    /// but writes nothing to disk. The returned `RestoreOutcome` has
    /// `dry_run: true` so the frontend can render a preview dialog.
    pub dry_run: bool,

    /// When `false` (default), the store halts with
    /// [`RestoreResult::NeedsConfirmation`] if any file the rewound session
    /// touched has a current disk hash different from what the session
    /// last observed. The caller (UI) must prompt the user and retry with
    /// `confirm_conflicts: true` to proceed.
    pub confirm_conflicts: bool,
}

/// Outcome of a [`CheckpointStore::restore`] call. The two-variant shape
/// forces callers to handle the conflict case explicitly — you can't
/// accidentally overwrite another session's work by calling `restore` with
/// default options.
#[derive(Debug, Clone)]
pub enum RestoreResult {
    /// The restore applied cleanly (or `dry_run: true` — no disk writes
    /// but the outcome describes what would have changed).
    Applied(RestoreOutcome),

    /// One or more files the rewound session touched have been modified
    /// elsewhere since this session last observed them. The caller must
    /// prompt the user and retry with `confirm_conflicts: true` to
    /// proceed; the store itself never auto-confirms.
    NeedsConfirmation(ConflictReport),
}

#[derive(Debug, Clone, Default)]
pub struct RestoreOutcome {
    /// Relative paths (to the session's root) that were (or would be)
    /// restored to their pre-turn content.
    pub paths_restored: Vec<String>,

    /// Relative paths that were (or would be) deleted because they were
    /// created during the rewound span.
    pub paths_deleted: Vec<String>,

    /// Relative paths the rewound session touched but for which we don't
    /// have a captured pre-state blob (e.g. the file was first-touched in
    /// the same turn we're rewinding to, with no earlier checkpoint).
    /// These files are LEFT ALONE on disk — we can't restore them because
    /// the pre-state was never observed. The UI surfaces these as
    /// "couldn't restore".
    pub paths_skipped: Vec<String>,

    /// Mirrors [`RestoreOptions::dry_run`] on the request side so the
    /// caller can distinguish "these paths changed" from "these paths
    /// would change".
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ConflictReport {
    pub conflicts: Vec<ConflictPath>,
}

#[derive(Debug, Clone)]
pub struct ConflictPath {
    /// Relative to the session's root.
    pub path: String,

    /// Hash of the file's content the last time THIS session observed it.
    /// `None` means the session expected the file not to exist here.
    pub session_last_seen_hash: Option<BlobHash>,

    /// Hash of the file on disk right now. `None` means the file does not
    /// currently exist.
    pub disk_current_hash: Option<BlobHash>,
}

#[derive(Debug, Clone, Default)]
pub struct GcReport {
    pub blobs_deleted: usize,
    pub manifests_deleted: usize,
    pub cache_rows_deleted: usize,
}

/// Public contract between `runtime-core` and whichever concrete checkpoint
/// implementation the daemon is configured with.
///
/// Implementations MUST be idempotent on [`Self::capture`] — calling twice
/// with the same `(session_id, turn_id)` returns the same handle and does
/// not create a duplicate manifest. The uniqueness constraint is enforced
/// in the sqlite index (see PR 2) plus atomic temp-then-rename writes.
///
/// All methods take `&self`; implementations handle their own interior
/// mutability (the default impl uses `parking_lot::Mutex` on the sqlite
/// connection plus `tokio::fs` for blob I/O).
#[async_trait]
pub trait CheckpointStore: Send + Sync + 'static {
    /// Capture the current state of `root` for the given turn. Called by
    /// `runtime-core` at turn end, before persisting the turn record.
    ///
    /// Returns:
    /// - `Ok(Some(handle))` on a successful capture. The caller publishes
    ///   `CheckpointCaptured` so the UI can enable the rewind affordance
    ///   for this turn.
    /// - `Ok(None)` if the store deliberately skipped (no `cwd`, or
    ///   enablement flag off).
    /// - `Err(_)` on any unrecoverable error. The caller logs and
    ///   proceeds — capture failure is never fatal to the turn itself.
    async fn capture(
        &self,
        session_id: &str,
        turn_id: &str,
        root: &Path,
    ) -> Result<Option<CheckpointHandle>, CheckpointError>;

    /// Restore the workspace at `root` to the state this session saw just
    /// before `target_turn_id`. See [`RestoreOptions`] for dry-run and
    /// conflict-confirmation semantics.
    async fn restore(
        &self,
        session_id: &str,
        target_turn_id: &str,
        root: &Path,
        options: RestoreOptions,
    ) -> Result<RestoreResult, CheckpointError>;

    /// Return `true` iff a checkpoint has been recorded for this
    /// `(session_id, turn_id)`. Used by the frontend to enable / hide the
    /// per-turn rewind affordance without an extra events-log scan.
    async fn has_checkpoint(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<bool, CheckpointError>;

    /// Delete all checkpoints, manifests, and cache entries associated
    /// with `session_id`. Called on session delete and — separately —
    /// from the worktree destroy hook in the Tauri app layer. Safe to call
    /// multiple times.
    async fn delete_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), CheckpointError>;

    /// Sweep the blob store and remove blobs that are no longer referenced
    /// by any live manifest, plus any manifest files with no
    /// corresponding index row. The default bootstrap path runs this
    /// periodically; `delete_for_session` can trigger it eagerly so
    /// worktree destroy reclaims disk synchronously.
    async fn collect_garbage(&self) -> Result<GcReport, CheckpointError>;
}

// -----------------------------------------------------------------------
// NoopCheckpointStore — for tests and the "disabled at runtime" path.
// -----------------------------------------------------------------------

/// A [`CheckpointStore`] that does nothing. Used in two places:
///
/// 1. `runtime-core` integration tests that don't exercise the rewind path
///    and don't want a real store + temp dir in every test.
/// 2. The optional "globally disable checkpoints" mode — when the user
///    flips the global setting to `false`, the daemon bootstrap swaps in
///    this impl so no capture cost is incurred. (An alternative is to let
///    the real store respect the flag internally; see PR 5.5.)
///
/// Ignores every input. `restore` always returns an `Applied` outcome
/// with empty paths — which is a visible "no-op" to the caller rather
/// than an error.
#[derive(Debug, Default)]
pub struct NoopCheckpointStore;

#[async_trait]
impl CheckpointStore for NoopCheckpointStore {
    async fn capture(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _root: &Path,
    ) -> Result<Option<CheckpointHandle>, CheckpointError> {
        Ok(None)
    }

    async fn restore(
        &self,
        _session_id: &str,
        _target_turn_id: &str,
        _root: &Path,
        options: RestoreOptions,
    ) -> Result<RestoreResult, CheckpointError> {
        Ok(RestoreResult::Applied(RestoreOutcome {
            dry_run: options.dry_run,
            ..Default::default()
        }))
    }

    async fn has_checkpoint(
        &self,
        _session_id: &str,
        _turn_id: &str,
    ) -> Result<bool, CheckpointError> {
        Ok(false)
    }

    async fn delete_for_session(
        &self,
        _session_id: &str,
    ) -> Result<(), CheckpointError> {
        Ok(())
    }

    async fn collect_garbage(&self) -> Result<GcReport, CheckpointError> {
        Ok(GcReport::default())
    }
}
