//! Sanity tests for `NoopCheckpointStore`. The real store lands in PR 2
//! with a much fuller test suite; this file exists so PR 1 has at least
//! one `cargo test -p zenui-checkpoints` target and the CI signal for
//! "does the crate compile and expose the expected API surface" is green.

use std::path::Path;

use crate::{CheckpointStore, NoopCheckpointStore, RestoreOptions, RestoreResult};

#[tokio::test]
async fn capture_returns_none() {
    let store = NoopCheckpointStore;
    let out = store
        .capture("s1", "t1", Path::new("/tmp"))
        .await
        .expect("noop capture never errors");
    assert!(out.is_none());
}

#[tokio::test]
async fn restore_applies_empty_outcome() {
    let store = NoopCheckpointStore;
    let result = store
        .restore(
            "s1",
            "t1",
            Path::new("/tmp"),
            RestoreOptions {
                dry_run: true,
                confirm_conflicts: false,
            },
        )
        .await
        .expect("noop restore never errors");
    match result {
        RestoreResult::Applied(outcome) => {
            assert!(outcome.dry_run);
            assert!(outcome.paths_restored.is_empty());
            assert!(outcome.paths_deleted.is_empty());
            assert!(outcome.paths_skipped.is_empty());
        }
        RestoreResult::NeedsConfirmation(_) => {
            panic!("noop restore should never surface conflicts");
        }
    }
}

#[tokio::test]
async fn has_checkpoint_is_always_false() {
    let store = NoopCheckpointStore;
    assert!(!store.has_checkpoint("s1", "t1").await.unwrap());
}

#[tokio::test]
async fn delete_and_gc_are_noops() {
    let store = NoopCheckpointStore;
    store.delete_for_session("s1").await.unwrap();
    let report = store.collect_garbage().await.unwrap();
    assert_eq!(report.blobs_deleted, 0);
    assert_eq!(report.manifests_deleted, 0);
    assert_eq!(report.cache_rows_deleted, 0);
}
