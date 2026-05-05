//! Per-session long-lived subprocess cache with an idle-kill watchdog.
//!
//! Two provider adapters (`provider-claude-sdk`,
//! `provider-github-copilot`) each cache a long-running bridge/CLI
//! child per session so the SDK's in-memory
//! context is reused across turns instead of paid for on every turn
//! startup. All three independently grew the same state machine:
//!
//! - a `HashMap<SessionId, CachedProcess<T>>` under a `Mutex`
//! - an `AtomicU64` last-activity stamp per entry
//! - an `AtomicU32` in-flight counter per entry
//! - an RAII guard that stamps activity on drop
//! - a background task that scans the map every N seconds and kills
//!   entries whose `in_flight == 0` and whose activity stamp is older
//!   than the idle threshold
//!
//! Having three copies drift is already how bugs happen (one had a
//! pending-rpcs map hook baked in, the others didn't; their idle/tick
//! constants diverged). This module lifts the generic piece. Each
//! adapter keeps whatever auxiliary state is truly provider-specific
//! (claude-sdk's `pending_rpcs`, for example) as separate fields.
//!
//! # Usage sketch
//!
//! ```ignore
//! let cache: ProcessCache<MyBridgeProcess> = ProcessCache::new(
//!     BRIDGE_IDLE_TIMEOUT_SECS,
//!     BRIDGE_WATCHDOG_INTERVAL_SECS,
//!     "provider-mybridge",
//! );
//!
//! // On every turn:
//! let entry = cache.get_or_insert_with(session_id, || async {
//!     let child = spawn_bridge().await?;
//!     Ok(MyBridgeProcess { child, stdin, .. })
//! }).await?;
//! let _activity = entry.activity_guard();
//! // ... run turn against entry.inner().lock().await ...
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tracing::info;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A cached child process plus the atomics the watchdog reads. Clone
/// the `Arc` to share ownership between callers and the watchdog. The
/// inner `Mutex<T>` is held only while actually using the child — the
/// activity atomics live outside it so the watchdog can inspect them
/// without contending for the process lock.
#[derive(Debug)]
pub struct CachedProcess<T> {
    inner: Arc<Mutex<T>>,
    last_activity: Arc<AtomicU64>,
    in_flight: Arc<AtomicU32>,
}

impl<T> Clone for CachedProcess<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            last_activity: self.last_activity.clone(),
            in_flight: self.in_flight.clone(),
        }
    }
}

impl<T> CachedProcess<T> {
    /// Shared handle to the underlying child process state. Lock it
    /// while writing to stdin, reading from stdout, etc.
    pub fn inner(&self) -> &Arc<Mutex<T>> {
        &self.inner
    }

    /// Bump the in-flight counter and return a guard that'll decrement
    /// it (and stamp activity) when dropped. Hold this for the lifetime
    /// of a turn.
    pub fn activity_guard(&self) -> ActivityGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        ActivityGuard {
            in_flight: self.in_flight.clone(),
            last_activity: self.last_activity.clone(),
        }
    }
}

/// RAII guard held for the duration of a turn. On drop, decrements the
/// in-flight counter and stamps `last_activity = now`, starting the
/// idle clock.
pub struct ActivityGuard {
    in_flight: Arc<AtomicU32>,
    last_activity: Arc<AtomicU64>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.last_activity.store(unix_now(), Ordering::Release);
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Map of session id → cached process with an idle-kill watchdog.
/// See module docs.
pub struct ProcessCache<T: Send + Sync + 'static> {
    map: Arc<Mutex<HashMap<String, CachedProcess<T>>>>,
    idle_timeout_secs: u64,
    watchdog_interval_secs: u64,
    watchdog_started: Arc<AtomicBool>,
    log_target: &'static str,
}

impl<T: Send + Sync + 'static> ProcessCache<T> {
    pub fn new(
        idle_timeout_secs: u64,
        watchdog_interval_secs: u64,
        log_target: &'static str,
    ) -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout_secs,
            watchdog_interval_secs,
            watchdog_started: Arc::new(AtomicBool::new(false)),
            log_target,
        }
    }

    /// Insert or replace the entry for `session_id`. Returns the new
    /// `CachedProcess` handle. Any existing entry is dropped (caller is
    /// responsible for killing that child before calling this if the
    /// previous process should not outlive the cache slot).
    pub async fn insert(&self, session_id: String, process: T) -> CachedProcess<T> {
        let cached = CachedProcess {
            inner: Arc::new(Mutex::new(process)),
            last_activity: Arc::new(AtomicU64::new(unix_now())),
            in_flight: Arc::new(AtomicU32::new(0)),
        };
        let mut map = self.map.lock().await;
        map.insert(session_id, cached.clone());
        cached
    }

    /// Look up an existing entry without inserting.
    pub async fn get(&self, session_id: &str) -> Option<CachedProcess<T>> {
        self.map.lock().await.get(session_id).cloned()
    }

    /// Remove and return the entry for `session_id`, if any. Caller is
    /// responsible for killing the child process after removal.
    pub async fn remove(&self, session_id: &str) -> Option<CachedProcess<T>> {
        self.map.lock().await.remove(session_id)
    }

    /// Remove every entry and return them. Used by the owning
    /// adapter's `ProviderAdapter::shutdown` during daemon teardown to
    /// kill every cached child in one sweep — the adapter then locks
    /// each returned entry's inner process and calls `start_kill`
    /// (matching what the watchdog's `kill` callback does on per-entry
    /// expiry).
    ///
    /// This is *not* the same as `ensure_watchdog`'s per-tick cull: it
    /// ignores `in_flight` and `last_activity` because a daemon
    /// shutdown is authoritative — nothing new will run against these
    /// children, so holding them open for an active turn would just
    /// leak the subprocess past the daemon's exit.
    pub async fn drain_all(&self) -> Vec<(String, CachedProcess<T>)> {
        let mut map = self.map.lock().await;
        map.drain().collect()
    }

    /// Spawn the idle-kill watchdog exactly once.
    ///
    /// The watchdog ticks every `watchdog_interval_secs`, scans the
    /// cache, and removes any entry whose `in_flight == 0` and whose
    /// `last_activity` is older than `idle_timeout_secs`. For each
    /// removed entry, `kill` is called to terminate the child.
    ///
    /// `kill` is expected to be cheap and best-effort; errors are
    /// logged but do not stop the watchdog. Typical implementation:
    /// `|entry| async move { let mut p = entry.inner().lock().await; let _ = p.child.start_kill(); }`.
    pub fn ensure_watchdog<F, Fut>(&self, kill: F)
    where
        F: Fn(CachedProcess<T>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        if self.watchdog_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let map = self.map.clone();
        let idle_timeout = self.idle_timeout_secs;
        let interval = self.watchdog_interval_secs;
        let log_target = self.log_target;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(interval));
            // Consume the immediate first tick so we don't cull on boot.
            tick.tick().await;
            loop {
                tick.tick().await;
                let now = unix_now();
                let victims: Vec<(String, CachedProcess<T>)> = {
                    let mut m = map.lock().await;
                    let stale: Vec<String> = m
                        .iter()
                        .filter(|(_, c)| {
                            c.in_flight.load(Ordering::Acquire) == 0
                                && now.saturating_sub(c.last_activity.load(Ordering::Acquire))
                                    > idle_timeout
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    stale
                        .into_iter()
                        .filter_map(|k| m.remove(&k).map(|c| (k, c)))
                        .collect()
                };
                for (sid, cached) in victims {
                    info!(
                        target: "process-cache",
                        adapter = log_target,
                        session_id = %sid,
                        "bridge idle {}s, killing",
                        idle_timeout
                    );
                    kill(cached).await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn activity_guard_tracks_in_flight() {
        let cache = ProcessCache::<u32>::new(60, 30, "test");
        let entry = cache.insert("s1".to_string(), 0).await;
        assert_eq!(entry.in_flight.load(Ordering::Acquire), 0);
        {
            let _g = entry.activity_guard();
            assert_eq!(entry.in_flight.load(Ordering::Acquire), 1);
            let _g2 = entry.activity_guard();
            assert_eq!(entry.in_flight.load(Ordering::Acquire), 2);
        }
        assert_eq!(entry.in_flight.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn insert_get_remove_round_trip() {
        let cache = ProcessCache::<String>::new(60, 30, "test");
        cache.insert("a".into(), "hello".into()).await;
        assert!(cache.get("a").await.is_some());
        assert!(cache.get("b").await.is_none());
        let removed = cache.remove("a").await;
        assert!(removed.is_some());
        assert!(cache.get("a").await.is_none());
    }

    // Paused virtual clock: the watchdog uses a 1-second `tokio::time::interval`,
    // but `start_paused` auto-advances when the runtime is idle, so the test
    // runs in near-zero real time instead of paying a full second of wall-clock
    // sleep for the second tick.
    #[tokio::test(start_paused = true)]
    async fn watchdog_culls_stale_entries() {
        // idle-timeout 0 means "anything not currently in flight is stale".
        let cache = Arc::new(ProcessCache::<u32>::new(0, 1, "test"));
        // Insert an idle entry with activity stamped in the past.
        let entry = cache.insert("s1".into(), 0).await;
        entry.last_activity.store(0, Ordering::Release);

        let kill_count = Arc::new(AtomicUsize::new(0));
        {
            let kc = kill_count.clone();
            cache.ensure_watchdog(move |_| {
                let kc = kc.clone();
                async move {
                    kc.fetch_add(1, Ordering::SeqCst);
                }
            });
        }

        // First tick is consumed on boot; watchdog kills on the second. With
        // paused time, sleeping past the second tick is instantaneous.
        tokio::time::sleep(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            kill_count.load(Ordering::SeqCst) >= 1,
            "watchdog did not run"
        );
        assert!(cache.get("s1").await.is_none());
    }
}
