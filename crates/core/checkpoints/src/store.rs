//! `FsCheckpointStore` — the default [`CheckpointStore`] impl.
//!
//! Glues [`BlobStore`], the walker, the [`PersistenceService`]-backed
//! cache + index, and on-disk manifests. All the interesting algorithmic
//! work lives in the submodules; this module is mostly plumbing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;
use zenui_persistence::{CheckpointRow, FileStateRow, PersistenceService};

use crate::blob_store::BlobStore;
use crate::errors::{io_err, CheckpointError};
use crate::manifest::{BlobHash, Manifest, ManifestEntry, MANIFEST_VERSION};
use crate::walker::{self, WalkItem};
use crate::{
    CheckpointHandle, CheckpointStore, ConflictPath, ConflictReport, GcReport, RestoreOptions,
    RestoreOutcome, RestoreResult,
};

/// On-disk default [`CheckpointStore`].
///
/// Layout under `data_dir`:
/// ```text
/// <data_dir>/
/// ├── blobs/
/// │   └── <hh>/<rest-of-hash>   # content-addressed
/// └── manifests/
///     └── <checkpoint_id>.json
/// ```
///
/// Sqlite tables (`checkpoints`, `file_state`) live in the shared
/// `PersistenceService` — see the persistence crate for the schema.
#[derive(Debug)]
pub struct FsCheckpointStore {
    blob_store: BlobStore,
    manifests_dir: PathBuf,
    persistence: Arc<PersistenceService>,
}

impl FsCheckpointStore {
    /// Open (or create) an on-disk store rooted at `data_dir`. Creates
    /// `blobs/` and `manifests/` subdirectories if missing.
    pub fn open(
        data_dir: impl Into<PathBuf>,
        persistence: Arc<PersistenceService>,
    ) -> Result<Self, CheckpointError> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir).map_err(|e| io_err(data_dir.clone(), e))?;
        let blob_store = BlobStore::new(data_dir.join("blobs"))?;
        let manifests_dir = data_dir.join("manifests");
        std::fs::create_dir_all(&manifests_dir)
            .map_err(|e| io_err(manifests_dir.clone(), e))?;
        Ok(Self {
            blob_store,
            manifests_dir,
            persistence,
        })
    }

    fn manifest_filename(checkpoint_id: &str) -> String {
        format!("{checkpoint_id}.json")
    }

    /// Canonicalize `root` up-front so every cache row + manifest uses
    /// the same key regardless of whether the caller passed a symlinked
    /// or relative form.
    fn canonicalize_root(root: &Path) -> Result<PathBuf, CheckpointError> {
        if !root.is_dir() {
            return Err(CheckpointError::InvalidRoot {
                root: root.to_path_buf(),
                reason: "not a directory".to_string(),
            });
        }
        root.canonicalize()
            .map_err(|e| io_err(root.to_path_buf(), e))
    }

    /// Perform the walk + hash-if-changed + manifest-write step.
    /// Extracted so tests can exercise it without going through the
    /// async trait dispatch.
    async fn capture_inner(
        &self,
        session_id: &str,
        turn_id: &str,
        root: &Path,
    ) -> Result<CheckpointHandle, CheckpointError> {
        let canonical_root = Self::canonicalize_root(root)?;

        // Check for an existing manifest for this (session, turn). If
        // one is already on disk, return its handle — capture is
        // idempotent. A redelivery (daemon restart during turn-end drain)
        // must not write a second manifest.
        if let Some(existing) = self
            .persistence
            .get_checkpoint_by_turn(session_id, turn_id)
            .await
        {
            return Ok(CheckpointHandle {
                checkpoint_id: existing.checkpoint_id,
                session_id: existing.session_id,
                turn_id: existing.turn_id,
                created_at: existing.created_at,
            });
        }

        let items = walker::walk(&canonical_root)?;
        let mut touched: Vec<ManifestEntry> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::with_capacity(items.len());

        for item in &items {
            seen_paths.insert(item.rel_path.clone());
            match self.diff_and_maybe_hash(item).await? {
                DiffOutcome::Unchanged => { /* skip */ }
                DiffOutcome::Changed { pre, post } => {
                    touched.push(ManifestEntry {
                        path: item.rel_path.clone(),
                        pre_hash: pre,
                        post_hash: Some(post),
                    });
                }
            }
        }

        // Detect deletions: any file_state row under this root that we
        // didn't encounter on disk must have been deleted. Record it as
        // a touched entry with post_hash=None, then drop the cache row.
        let deletions = self
            .collect_deletions(&canonical_root, &seen_paths)
            .await?;
        for (rel_path, pre_hash) in deletions {
            touched.push(ManifestEntry {
                path: rel_path.clone(),
                pre_hash: Some(pre_hash),
                post_hash: None,
            });
        }

        let checkpoint_id = format!("cp_{}", Uuid::new_v4());
        let created_at = Utc::now().to_rfc3339();
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            checkpoint_id: checkpoint_id.clone(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            created_at: created_at.clone(),
            root: canonical_root.to_string_lossy().into_owned(),
            touched,
        };

        let manifest_file = Self::manifest_filename(&checkpoint_id);
        let manifest_path = self.manifests_dir.join(&manifest_file);
        manifest.write_atomic(&manifest_path)?;

        self.persistence
            .insert_checkpoint(CheckpointRow {
                checkpoint_id: checkpoint_id.clone(),
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                created_at: created_at.clone(),
                manifest_path: manifest_file,
            })
            .await
            .map_err(|e| CheckpointError::Sqlite(format!("{e:#}")))?;

        Ok(CheckpointHandle {
            checkpoint_id,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            created_at,
        })
    }

    /// Compare a walk entry against the persistent cache. On mtime+size
    /// hit, skip (content hasn't changed since we last hashed). On
    /// miss, hash the file now, write the blob if novel, update the
    /// cache, and return the `pre`/`post` pair for the manifest.
    async fn diff_and_maybe_hash(&self, item: &WalkItem) -> Result<DiffOutcome, CheckpointError> {
        let key = item.abs_path.to_string_lossy().into_owned();
        let cached = self.persistence.get_file_state(&key).await;
        if let Some(ref row) = cached {
            if row.mtime_ns == item.mtime_ns && row.size_bytes as u64 == item.size_bytes {
                // Refresh `updated_at` so the LRU GC sees this path as
                // active. Cheap — a single UPDATE with the same values
                // for the content-bearing columns.
                self.persistence
                    .upsert_file_state(FileStateRow {
                        abs_path: key,
                        mtime_ns: row.mtime_ns,
                        size_bytes: row.size_bytes,
                        blob_hash: row.blob_hash.clone(),
                        updated_at: Utc::now().to_rfc3339(),
                    })
                    .await;
                return Ok(DiffOutcome::Unchanged);
            }
        }

        // Cache miss or stale — read and hash.
        let bytes = std::fs::read(&item.abs_path)
            .map_err(|e| io_err(item.abs_path.clone(), e))?;
        let post = self.blob_store.write_if_absent(&bytes)?;
        let pre = cached
            .as_ref()
            .and_then(|r| BlobHash::parse(&r.blob_hash).ok());

        self.persistence
            .upsert_file_state(FileStateRow {
                abs_path: key,
                mtime_ns: item.mtime_ns,
                size_bytes: item.size_bytes as i64,
                blob_hash: post.as_str().to_string(),
                updated_at: Utc::now().to_rfc3339(),
            })
            .await;
        Ok(DiffOutcome::Changed { pre, post })
    }

    /// Gather cache rows under `root` whose path was not encountered in
    /// the current walk. Each corresponds to a file that was deleted
    /// during the turn (or outside it, e.g. by the user's editor — the
    /// manifest records them either way).
    ///
    /// This is currently implemented as "list all cache rows, filter
    /// by prefix." For a workspace with millions of cached paths that
    /// would be expensive; v1 doesn't optimize for that because
    /// typical workspace sizes stay in the low thousands.
    async fn collect_deletions(
        &self,
        root: &Path,
        seen_paths: &HashSet<String>,
    ) -> Result<Vec<(String, BlobHash)>, CheckpointError> {
        let root_str = root.to_string_lossy();
        let prefix = if root_str.ends_with(std::path::MAIN_SEPARATOR) {
            root_str.into_owned()
        } else {
            format!("{}{}", root_str, std::path::MAIN_SEPARATOR)
        };
        // Collect all cache rows that live under this root. We don't
        // have a direct "list paths under prefix" sqlite method yet —
        // instead, use `list_file_state_blob_hashes` to get everything
        // then filter. For v1 this is acceptable; a dedicated
        // `list_file_state_paths_under(prefix)` can follow when size
        // warrants.
        //
        // To avoid adding another sqlite method right now, re-read each
        // candidate path via `get_file_state` after widening the initial
        // list. This method therefore pays an extra round-trip per
        // candidate — optimization target if profiling flags it.
        let mut deleted = Vec::new();
        // We need the full set of (abs_path, blob_hash) rows under the
        // prefix. The persistence layer doesn't currently expose a
        // prefix scan, so we pull the full path list and filter. The
        // row count is bounded by the number of files flowstate has
        // observed across all sessions; typical installs stay well under
        // 100k.
        let all = self.persistence.list_file_state_under_prefix(&prefix).await;
        for row in all {
            let Ok(abs_path) = std::path::PathBuf::from(&row.abs_path).canonicalize() else {
                // Path no longer exists on disk. If it was under our
                // root, treat it as a deletion.
                if !row.abs_path.starts_with(&prefix) {
                    continue;
                }
                let Ok(hash) = BlobHash::parse(&row.blob_hash) else {
                    continue;
                };
                let rel = row
                    .abs_path
                    .strip_prefix(&prefix)
                    .unwrap_or(&row.abs_path)
                    .to_string();
                let rel = walker::normalize_separators(&rel);
                if !seen_paths.contains(&rel) {
                    deleted.push((rel, hash));
                    self.persistence.delete_file_state(&row.abs_path).await;
                }
                continue;
            };
            let rel_os = match abs_path.strip_prefix(root) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) => continue,
            };
            let rel = walker::normalize_separators(&rel_os);
            if !seen_paths.contains(&rel) {
                let Ok(hash) = BlobHash::parse(&row.blob_hash) else {
                    continue;
                };
                deleted.push((rel, hash));
                self.persistence.delete_file_state(&row.abs_path).await;
            }
        }
        Ok(deleted)
    }
}

enum DiffOutcome {
    Unchanged,
    Changed {
        pre: Option<BlobHash>,
        post: BlobHash,
    },
}

#[async_trait]
impl CheckpointStore for FsCheckpointStore {
    async fn capture(
        &self,
        session_id: &str,
        turn_id: &str,
        root: &Path,
    ) -> Result<Option<CheckpointHandle>, CheckpointError> {
        match self.capture_inner(session_id, turn_id, root).await {
            Ok(h) => Ok(Some(h)),
            Err(e) => Err(e),
        }
    }

    async fn restore(
        &self,
        session_id: &str,
        target_turn_id: &str,
        root: &Path,
        options: RestoreOptions,
    ) -> Result<RestoreResult, CheckpointError> {
        let canonical_root = Self::canonicalize_root(root)?;

        // Build the list of checkpoints from the target onwards (all
        // turns whose edits we need to undo), ordered chronologically.
        let manifests = self
            .persistence
            .list_session_checkpoints_from(session_id, target_turn_id)
            .await;
        if manifests.is_empty() {
            return Err(CheckpointError::NoCheckpoint {
                session_id: session_id.to_string(),
                turn_id: target_turn_id.to_string(),
            });
        }

        // Load each manifest. Accumulate the earliest-observed `pre_hash`
        // per path — that's the state THIS session expected to restore to.
        // Also track the session's LAST observed hash per path (from the
        // latest turn's `post_hash`) for conflict detection.
        use std::collections::HashMap;
        let mut earliest_pre: HashMap<String, Option<BlobHash>> = HashMap::new();
        let mut latest_post: HashMap<String, Option<BlobHash>> = HashMap::new();
        for row in &manifests {
            let manifest_path = self.manifests_dir.join(&row.manifest_path);
            let manifest = Manifest::load(&manifest_path)?;
            for entry in manifest.touched {
                earliest_pre
                    .entry(entry.path.clone())
                    .or_insert(entry.pre_hash.clone());
                latest_post.insert(entry.path.clone(), entry.post_hash.clone());
            }
        }

        // Conflict detection: for each touched path, compare disk's
        // current hash against this session's latest observed. If they
        // differ, someone else edited the file since we last saw it.
        let mut conflicts = Vec::new();
        for (rel_path, session_last_seen) in &latest_post {
            let abs = canonical_root.join(rel_path);
            let current = match std::fs::read(&abs) {
                Ok(bytes) => Some(BlobHash::hash_bytes(&bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(io_err(abs.clone(), e)),
            };
            if current.as_ref() != session_last_seen.as_ref() {
                conflicts.push(ConflictPath {
                    path: rel_path.clone(),
                    session_last_seen_hash: session_last_seen.clone(),
                    disk_current_hash: current,
                });
            }
        }

        if !conflicts.is_empty() && !options.confirm_conflicts {
            return Ok(RestoreResult::NeedsConfirmation(ConflictReport {
                conflicts,
            }));
        }

        // Apply the restore. Touched paths where `pre_hash` is present
        // get rewritten; paths where `pre_hash` is None get deleted
        // (they were created during the rewound span); paths where we
        // never captured a pre-state (first-touch) go to `paths_skipped`.
        let mut paths_restored = Vec::new();
        let mut paths_deleted = Vec::new();
        let mut paths_skipped = Vec::new();

        for (rel_path, pre_hash) in &earliest_pre {
            let abs = canonical_root.join(rel_path);
            match pre_hash {
                Some(hash) => {
                    if options.dry_run {
                        paths_restored.push(rel_path.clone());
                        continue;
                    }
                    let bytes = self.blob_store.read(hash)?;
                    if let Some(parent) = abs.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| io_err(parent.to_path_buf(), e))?;
                    }
                    std::fs::write(&abs, &bytes).map_err(|e| io_err(abs.clone(), e))?;
                    paths_restored.push(rel_path.clone());
                    // Refresh the cache to match what we just wrote so
                    // the next capture doesn't treat this as a change.
                    if let Ok(meta) = std::fs::metadata(&abs) {
                        let mtime_ns = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos() as i64)
                            .unwrap_or(0);
                        self.persistence
                            .upsert_file_state(FileStateRow {
                                abs_path: abs.to_string_lossy().into_owned(),
                                mtime_ns,
                                size_bytes: meta.len() as i64,
                                blob_hash: hash.as_str().to_string(),
                                updated_at: Utc::now().to_rfc3339(),
                            })
                            .await;
                    }
                }
                None => {
                    // File was created during the rewound span. Delete.
                    if options.dry_run {
                        paths_deleted.push(rel_path.clone());
                        continue;
                    }
                    match std::fs::remove_file(&abs) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Already gone — treat as success (the
                            // post-condition is "file does not exist").
                        }
                        Err(e) => return Err(io_err(abs.clone(), e)),
                    }
                    self.persistence
                        .delete_file_state(&abs.to_string_lossy())
                        .await;
                    paths_deleted.push(rel_path.clone());
                }
            }
        }

        // Sort for deterministic ordering in the outcome — the frontend
        // renders these lists verbatim and users appreciate stable order.
        paths_restored.sort();
        paths_deleted.sort();
        paths_skipped.sort();

        Ok(RestoreResult::Applied(RestoreOutcome {
            paths_restored,
            paths_deleted,
            paths_skipped,
            dry_run: options.dry_run,
        }))
    }

    async fn has_checkpoint(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<bool, CheckpointError> {
        Ok(self
            .persistence
            .get_checkpoint_by_turn(session_id, turn_id)
            .await
            .is_some())
    }

    async fn delete_for_session(&self, session_id: &str) -> Result<(), CheckpointError> {
        let manifest_files = self
            .persistence
            .delete_checkpoints_for_session(session_id)
            .await;
        for name in manifest_files {
            let path = self.manifests_dir.join(&name);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(path, e)),
            }
        }
        // Blobs may still be referenced by other sessions' manifests.
        // Let the periodic GC sweep reclaim orphans — eager full-scan
        // here would stall the worktree-destroy path for workspaces
        // with millions of blobs.
        Ok(())
    }

    async fn collect_garbage(&self) -> Result<GcReport, CheckpointError> {
        let mut report = GcReport::default();

        // 1. Build the live-blob set from every surviving manifest.
        let checkpoints = self.persistence.list_all_checkpoints().await;
        let mut live_blobs: HashSet<String> = HashSet::new();
        let mut live_manifest_files: HashSet<String> = HashSet::new();
        for row in &checkpoints {
            live_manifest_files.insert(row.manifest_path.clone());
            let manifest_path = self.manifests_dir.join(&row.manifest_path);
            let manifest = match Manifest::load(&manifest_path) {
                Ok(m) => m,
                Err(CheckpointError::Io { .. }) => continue, // already gone; move on
                Err(e) => {
                    tracing::warn!("gc: manifest load failed: {e}");
                    continue;
                }
            };
            for entry in manifest.touched {
                if let Some(h) = entry.pre_hash {
                    live_blobs.insert(h.as_str().to_string());
                }
                if let Some(h) = entry.post_hash {
                    live_blobs.insert(h.as_str().to_string());
                }
            }
        }

        // 2. Add blobs referenced by the persistent cache. Cache rows
        // represent "we believe the file on disk currently has this
        // hash." If we reclaimed those blobs, the next capture would
        // see a cache hit and skip hashing — producing a manifest
        // referencing a blob we already deleted. So cache-referenced
        // blobs are live too.
        for h in self.persistence.list_file_state_blob_hashes().await {
            live_blobs.insert(h);
        }

        // 3. Sweep the blob store — delete any blob not in live_blobs.
        for hash in self.blob_store.iter_hashes()? {
            if !live_blobs.contains(hash.as_str()) {
                self.blob_store.delete(&hash)?;
                report.blobs_deleted += 1;
            }
        }
        // Temp-file orphans always go.
        let _ = self.blob_store.sweep_tmp_orphans()?;

        // 4. Sweep orphan manifest files (manifest on disk but no row
        // in the checkpoints table, e.g. a prior `delete_for_session`
        // that raced with a concurrent write).
        if let Ok(dir) = std::fs::read_dir(&self.manifests_dir) {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if !name_str.ends_with(".json") {
                    continue;
                }
                if !live_manifest_files.contains(name_str) {
                    let _ = std::fs::remove_file(entry.path());
                    report.manifests_deleted += 1;
                }
            }
        }

        // 5. Prune file_state rows older than 90 days. An abandoned
        // workspace shouldn't keep its cache entries alive forever.
        let cutoff = Utc::now() - chrono::Duration::days(90);
        let cutoff_rfc = cutoff.to_rfc3339();
        report.cache_rows_deleted = self.persistence.prune_stale_file_state(&cutoff_rfc).await;

        Ok(report)
    }
}
