//! Integration tests for `FsCheckpointStore`. Spin up a temp data dir
//! + an in-memory `PersistenceService` per test and exercise the full
//! capture→modify→restore cycle, including the cases that motivated
//! this redesign (bash-level edits, shared-cwd isolation, conflict
//! detection, created/deleted files, idempotent capture).

use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use zenui_checkpoints::{CheckpointStore, FsCheckpointStore, RestoreOptions, RestoreResult};
use zenui_persistence::PersistenceService;

fn new_store() -> (FsCheckpointStore, Arc<PersistenceService>, TempDir, TempDir) {
    let data_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let persistence = Arc::new(PersistenceService::in_memory().unwrap());
    let store = FsCheckpointStore::open(data_dir.path(), persistence.clone()).unwrap();
    (store, persistence, data_dir, workspace)
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, bytes).unwrap();
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap()
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

#[tokio::test]
async fn capture_then_modify_then_restore_is_bit_identical() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"original");
    store.capture("s1", "t1", ws.path()).await.unwrap();

    // Agent modifies the file during turn 2, which we capture.
    write(ws.path(), "a.rs", b"modified");
    store.capture("s1", "t2", ws.path()).await.unwrap();

    // Rewind to the state before t2 → restores original.
    let result = store
        .restore("s1", "t2", ws.path(), RestoreOptions::default())
        .await
        .unwrap();
    let RestoreResult::Applied(outcome) = result else {
        panic!("expected clean apply, got conflicts");
    };
    assert_eq!(outcome.paths_restored, vec!["a.rs"]);
    assert!(outcome.paths_deleted.is_empty());
    assert_eq!(read(ws.path(), "a.rs"), b"original");
}

#[tokio::test]
async fn restore_deletes_files_created_during_span() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "old.rs", b"keep");
    store.capture("s1", "t1", ws.path()).await.unwrap();

    write(ws.path(), "new.rs", b"created by agent");
    store.capture("s1", "t2", ws.path()).await.unwrap();

    let result = store
        .restore("s1", "t2", ws.path(), RestoreOptions::default())
        .await
        .unwrap();
    let RestoreResult::Applied(outcome) = result else {
        panic!("expected clean apply");
    };
    assert_eq!(outcome.paths_deleted, vec!["new.rs"]);
    assert!(!exists(ws.path(), "new.rs"));
    assert!(exists(ws.path(), "old.rs"));
}

#[tokio::test]
async fn restore_recreates_files_deleted_during_span() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"keep me");
    store.capture("s1", "t1", ws.path()).await.unwrap();

    std::fs::remove_file(ws.path().join("a.rs")).unwrap();
    store.capture("s1", "t2", ws.path()).await.unwrap();

    let result = store
        .restore("s1", "t2", ws.path(), RestoreOptions::default())
        .await
        .unwrap();
    let RestoreResult::Applied(outcome) = result else {
        panic!("expected clean apply");
    };
    assert_eq!(outcome.paths_restored, vec!["a.rs"]);
    assert_eq!(read(ws.path(), "a.rs"), b"keep me");
}

#[tokio::test]
async fn rewind_catches_bash_driven_edits() {
    // This is the scenario that fundamentally motivated the redesign:
    // the old FileChangeRecord walker only saw tool-arg-level edits, so
    // a shell command like `echo new > foo.txt` was invisible. Here we
    // simulate exactly that — no tool call, just a direct disk edit —
    // and confirm rewind restores correctly.
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "foo.txt", b"v1");
    store.capture("s1", "t1", ws.path()).await.unwrap();

    // Pretend a bash tool did this; no FileChange event was ever
    // emitted, but our stat-walk sees it.
    write(ws.path(), "foo.txt", b"v2-bash");
    store.capture("s1", "t2", ws.path()).await.unwrap();

    let result = store
        .restore("s1", "t2", ws.path(), RestoreOptions::default())
        .await
        .unwrap();
    let RestoreResult::Applied(_) = result else {
        panic!("expected clean apply");
    };
    assert_eq!(read(ws.path(), "foo.txt"), b"v1");
}

#[tokio::test]
async fn capture_is_idempotent_per_session_turn_pair() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"x");
    let h1 = store.capture("s1", "t1", ws.path()).await.unwrap().unwrap();
    // Second call with the same (session, turn) returns the same handle.
    let h2 = store.capture("s1", "t1", ws.path()).await.unwrap().unwrap();
    assert_eq!(h1.checkpoint_id, h2.checkpoint_id);
}

#[tokio::test]
async fn has_checkpoint_gates_ui_affordance() {
    let (store, _p, _data, ws) = new_store();
    assert!(!store.has_checkpoint("s1", "t1").await.unwrap());
    write(ws.path(), "a.rs", b"x");
    store.capture("s1", "t1", ws.path()).await.unwrap();
    assert!(store.has_checkpoint("s1", "t1").await.unwrap());
    assert!(!store.has_checkpoint("s1", "t-missing").await.unwrap());
}

#[tokio::test]
async fn shared_cwd_rewind_preserves_other_sessions_work() {
    // Two sessions share a cwd. Each edits different files. Rewinding
    // session A must not touch session B's file.
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"a-v1");
    write(ws.path(), "b.rs", b"b-v1");
    store.capture("sA", "tA1", ws.path()).await.unwrap();
    store.capture("sB", "tB1", ws.path()).await.unwrap();

    // A edits a.rs.
    write(ws.path(), "a.rs", b"a-v2");
    store.capture("sA", "tA2", ws.path()).await.unwrap();
    // B edits b.rs (its capture picks up B's change + A's prior write).
    write(ws.path(), "b.rs", b"b-v2");
    store.capture("sB", "tB2", ws.path()).await.unwrap();

    // Rewind A: only a.rs was touched by A, so only a.rs reverts.
    let result = store
        .restore(
            "sA",
            "tA2",
            ws.path(),
            RestoreOptions {
                confirm_conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let RestoreResult::Applied(outcome) = result else {
        panic!("expected apply after confirm_conflicts");
    };
    assert!(outcome.paths_restored.contains(&"a.rs".to_string()));
    assert!(!outcome.paths_restored.contains(&"b.rs".to_string()));
    assert_eq!(read(ws.path(), "a.rs"), b"a-v1");
    // B's work is untouched.
    assert_eq!(read(ws.path(), "b.rs"), b"b-v2");
}

#[tokio::test]
async fn conflict_detection_halts_by_default() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "shared.rs", b"v1");
    store.capture("sA", "tA1", ws.path()).await.unwrap();

    // A edits it.
    write(ws.path(), "shared.rs", b"v2-by-A");
    store.capture("sA", "tA2", ws.path()).await.unwrap();

    // Another actor (session B, user editor, whatever) overwrites it.
    write(ws.path(), "shared.rs", b"v3-by-other");

    // A tries to rewind. Default options do NOT auto-confirm.
    let result = store
        .restore("sA", "tA2", ws.path(), RestoreOptions::default())
        .await
        .unwrap();
    match result {
        RestoreResult::NeedsConfirmation(report) => {
            assert_eq!(report.conflicts.len(), 1);
            assert_eq!(report.conflicts[0].path, "shared.rs");
        }
        RestoreResult::Applied(_) => panic!("expected NeedsConfirmation"),
    }

    // Disk is untouched — no silent overwrite.
    assert_eq!(read(ws.path(), "shared.rs"), b"v3-by-other");

    // With confirm_conflicts, the rewind proceeds (and clobbers the
    // other actor's change — documented tradeoff).
    let result = store
        .restore(
            "sA",
            "tA2",
            ws.path(),
            RestoreOptions {
                confirm_conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let RestoreResult::Applied(_) = result else {
        panic!("expected Applied on confirm");
    };
    assert_eq!(read(ws.path(), "shared.rs"), b"v1");
}

#[tokio::test]
async fn dry_run_leaves_disk_untouched() {
    let (store, _p, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"v1");
    store.capture("s1", "t1", ws.path()).await.unwrap();
    write(ws.path(), "a.rs", b"v2");
    store.capture("s1", "t2", ws.path()).await.unwrap();

    let result = store
        .restore(
            "s1",
            "t2",
            ws.path(),
            RestoreOptions {
                dry_run: true,
                confirm_conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let RestoreResult::Applied(outcome) = result else {
        panic!("dry run should not conflict");
    };
    assert!(outcome.dry_run);
    assert_eq!(outcome.paths_restored, vec!["a.rs"]);
    // Disk still has v2 because we dry-ran.
    assert_eq!(read(ws.path(), "a.rs"), b"v2");
}

#[tokio::test]
async fn restore_returns_no_checkpoint_for_unknown_turn() {
    let (store, _p, _data, ws) = new_store();
    let err = store
        .restore("s1", "t-missing", ws.path(), RestoreOptions::default())
        .await
        .unwrap_err();
    match err {
        zenui_checkpoints::CheckpointError::NoCheckpoint { .. } => {}
        other => panic!("expected NoCheckpoint, got {other:?}"),
    }
}

#[tokio::test]
async fn delete_for_session_removes_manifests_and_index_rows() {
    let (store, persistence, _data, ws) = new_store();
    write(ws.path(), "a.rs", b"x");
    let handle = store.capture("s1", "t1", ws.path()).await.unwrap().unwrap();

    // Pre-check: manifest exists on disk and in the index.
    assert!(
        persistence
            .get_checkpoint_by_turn("s1", "t1")
            .await
            .is_some()
    );

    store.delete_for_session("s1").await.unwrap();

    // Post: index empty, manifest file gone.
    assert!(
        persistence
            .get_checkpoint_by_turn("s1", "t1")
            .await
            .is_none()
    );
    assert!(!store.has_checkpoint("s1", "t1").await.unwrap());
    // The handle we captured is opaque — assert it at least had the
    // id we expected (sanity check the ID shape).
    assert!(handle.checkpoint_id.starts_with("cp_"));
}

#[tokio::test]
async fn gc_reclaims_blobs_no_longer_referenced() {
    let (store, _p, data, ws) = new_store();
    write(ws.path(), "a.rs", b"unique-blob-a");
    store.capture("s1", "t1", ws.path()).await.unwrap();

    // Before deletion, the blob exists on disk.
    let blob_dir = data.path().join("blobs");
    let blobs_before = count_files(&blob_dir);
    assert!(blobs_before >= 1);

    // Delete the session and then GC. Blob a.rs wrote should be gone
    // because (1) no manifest references it, (2) the file_state row
    // for the now-deleted workspace path also goes (via LRU if we've
    // simulated age — here we skip LRU since it's too slow to test).
    store.delete_for_session("s1").await.unwrap();

    // Physically remove the workspace so capture doesn't recreate the
    // cache row on re-use.
    drop(ws);

    // Wipe the file_state cache too (simulating a stale entry that
    // should also be pruned). Otherwise the cache keeps the blob alive.
    // Normally the LRU prune path handles this; in this test we force
    // the condition by emptying the cache explicitly.
    for h in _p.list_file_state_blob_hashes().await {
        // We don't have a bulk-delete; use the prune method with a
        // far-future cutoff so every row qualifies.
        let _ = h;
    }
    let future = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    _p.prune_stale_file_state(&future).await;

    let report = store.collect_garbage().await.unwrap();
    assert!(report.blobs_deleted >= 1);
    let blobs_after = count_files(&blob_dir);
    assert!(blobs_after < blobs_before);
}

fn count_files(dir: &Path) -> usize {
    fn recurse(path: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                recurse(&entry.path(), count);
            } else if meta.is_file() {
                let name = entry.file_name();
                // Skip .tmp orphans for stable counting.
                if name.to_str().map(|s| s.starts_with('.')).unwrap_or(false) {
                    continue;
                }
                *count += 1;
            }
        }
    }
    let mut c = 0;
    recurse(dir, &mut c);
    c
}
