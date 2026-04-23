//! opencode provider adapter.
//!
//! Drives the opencode coding agent via its headless HTTP server
//! (`opencode serve`) with Server-Sent Events for streaming. The
//! server is spawned once per daemon on first use and shared across
//! every flowstate session that picks the opencode provider —
//! opencode's server is natively multi-session, so one process is
//! enough no matter how many chats the user has open.
//!
//! Module layout:
//! - [`server`] — child-process lifecycle: pick a free localhost
//!   port, generate a random password, spawn `opencode serve`, wait
//!   for its readiness line, tear down cleanly on drop.
//! - [`http`]   — thin `reqwest` wrappers over the REST endpoints we
//!   actually consume (session create, prompt, abort, health, model
//!   catalog, permission answers).
//! - [`events`] — SSE reader against `/event`; parses the JSON
//!   payloads and forwards per-session events to whichever
//!   `TurnEventSink` is currently active for that session id.
//!
//! The public entry point is [`OpenCodeAdapter`], constructed in the
//! host app's adapter vector alongside the other providers.

mod events;
mod http;
mod server;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use zenui_provider_api::{
    OrchestrationIpcHandle, OrchestrationIpcInfo, PermissionMode, ProviderAdapter, ProviderKind,
    ProviderModel, ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnOutput,
    ReasoningEffort, SessionDetail, SharedBridgeGuard, ThinkingMode, TurnEventSink, UserInput,
    find_cli_binary,
};

use crate::events::EventRouter;
use crate::http::OpenCodeClient;
use crate::server::OpenCodeServer;

/// Install hint surfaced in `health()` when the `opencode` binary is
/// missing. Kept as a constant so the diagnostic message stays
/// consistent between the pre-flight probe and any runtime errors.
const OPENCODE_INSTALL_HINT: &str =
    "Install opencode. macOS: `brew install sst/tap/opencode`  \u{2022}  \
     Linux/macOS: `curl -fsSL https://opencode.ai/install | bash`  \u{2022}  \
     any platform: `npm i -g opencode-ai`";

/// The binary name we look for on PATH. opencode ships as `opencode`
/// on every platform (no `.cmd` shim on Windows today — the installer
/// drops `opencode.exe`).
const OPENCODE_BINARY: &str = "opencode";

/// Bound an initial server startup + health probe. Opencode's own
/// readiness log usually lands in <1s; we give generous headroom for
/// cold starts (npm-installed shims on Windows can take a few
/// seconds) but bail before a stuck probe blocks the daemon's
/// bootstrap flow.
const SERVER_STARTUP_TIMEOUT_SECS: u64 = 10;

/// Effectively no turn-level wall clock. The adapter previously
/// enforced a 10-minute cap to guard against subprocess wedges, but
/// long legitimate agent runs (big refactors, multi-step builds)
/// routinely exceed that. Users cancel stuck turns manually via the
/// UI. `u32::MAX` seconds (~136 years) is the sentinel — large
/// enough that real turns never trip it, small enough that tokio's
/// `Instant::now() + Duration::from_secs(TURN_TIMEOUT_SECS)` math
/// won't overflow.
const TURN_TIMEOUT_SECS: u64 = u32::MAX as u64;

/// Sentinel `origin.session_id` used for every flowstate tool call
/// that comes through the shared opencode server's MCP subprocess.
///
/// Opencode's MCP config is global to the `opencode serve` process —
/// one `opencode.json` feeds every session that server hosts. Because
/// the flowstate MCP command array is baked into that config at
/// server startup and cannot be varied per session, a single session
/// id is hard-coded into it. Every opencode-agent's orchestration
/// tool call therefore arrives at the runtime with
/// `origin.session_id == OPENCODE_SHARED_SESSION_ID`.
///
/// Consequences to be aware of (acceptable for single-user desktop
/// use, may want a rethink for shared deployments):
///
/// - The runtime's per-origin orchestration budget (default 10
///   calls/turn — see `DEFAULT_TURN_BUDGET` in
///   `runtime-core::orchestration`) is shared across every
///   opencode session. Two concurrent opencode sessions each doing
///   heavy fan-out will exhaust the shared budget faster than an
///   isolated per-session budget would.
/// - The cycle-detection graph treats all opencode sessions as one
///   caller. Normal spawn/send patterns don't trip it; pathological
///   "opencode A spawns opencode B spawns opencode A" can false-
///   positive. MAX_AWAIT_DEPTH=4 gives headroom.
/// - Logs / telemetry keyed on origin.session_id will collapse
///   opencode-side activity onto one row.
///
/// The value intentionally doesn't look like a UUID so it's easy to
/// spot in logs and impossible to collide with a real session id.
const OPENCODE_SHARED_SESSION_ID: &str = "opencode-shared";

/// Key used in [`ProviderSessionState::metadata`] to tag an opencode
/// native session with the server generation that minted it. On each
/// respawn the adapter's `server_generation` increments; a cached
/// native id whose recorded generation doesn't match the current one
/// was issued by a now-dead `opencode serve` and must be recreated
/// (the new server knows nothing about that UUID).
///
/// Phase A: generation is always 0 because the server never respawns
/// at runtime. The plumbing exists so Phase B can enable idle-kill
/// without a second data-migration pass. Sessions that pre-date this
/// field (no metadata, or metadata without this key) are treated as
/// matching — forcing all existing sessions to recreate on upgrade
/// would needlessly churn conversation state.
const GENERATION_METADATA_KEY: &str = "opencode_generation";

/// Default idle TTL baked into the adapter. 3 minutes — long enough
/// to absorb "close a tab, open another" cycles without a cold
/// start, short enough that an idle laptop returns the opencode
/// child's memory promptly. Hosts can pass an explicit override via
/// `new_with_orchestration_and_idle_ttl` (zero disables idle-kill).
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(180);

/// Cadence at which the idle watcher re-checks the lease tracker
/// while the server is idle. Tighter than `DEFAULT_IDLE_TTL` so the
/// kill fires within a second of the TTL elapsing rather than one
/// TTL interval late on average.
const IDLE_CHECK_GRANULARITY: Duration = Duration::from_millis(500);

/// Tracks in-flight work against the shared opencode server. Each
/// active caller (an `execute_turn`, a `health()` probe, a
/// `fetch_models()` call, an MCP dispatch from `opencode-shared`,
/// etc.) holds a [`Lease`] for the duration of its work; when every
/// lease drops and enough idle time passes, the watcher tears the
/// child down.
///
/// Adapter-scoped (not per-server-generation) so the idle watcher
/// task can hold a single `Arc<LeaseTracker>` across respawns.
struct LeaseTracker {
    /// Number of outstanding leases. Incremented on [`Lease`] mint,
    /// decremented in the guard's `Drop`. `SeqCst` is overkill but
    /// this is never on a hot path.
    inflight: AtomicUsize,
    /// Wall-clock millis at which the lease count last dropped to
    /// zero. Phase B reads this to decide "has the server been idle
    /// *long enough*" — a lease that arrives just before the idle
    /// tick should not kill the server immediately on release.
    last_release_ms: AtomicI64,
    /// Pinged when `inflight` transitions to zero. Phase B's idle
    /// watcher awaits this to wake precisely on the last release
    /// rather than polling.
    idle_notify: Notify,
}

impl LeaseTracker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inflight: AtomicUsize::new(0),
            last_release_ms: AtomicI64::new(now_millis()),
            idle_notify: Notify::new(),
        })
    }

    /// Mint a new lease. Returns an RAII guard whose `Drop` releases.
    fn acquire(self: &Arc<Self>) -> Lease {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        Lease {
            tracker: Some(self.clone()),
        }
    }
}

/// RAII guard representing one unit of in-flight work against the
/// shared opencode server. Hold it for the whole duration of the
/// operation that needs the server alive; drop it the moment you're
/// done. Leasing through the [`LeaseTracker`] is the single choke
/// point Phase B's idle watcher relies on — if a code path touches
/// the server without a lease, the watcher can kill the child out
/// from under it.
///
/// Phase A: leases are minted and dropped correctly but the counter
/// is never read. The behaviour is identical to the pre-refactor
/// code.
pub(crate) struct Lease {
    /// `Option` so `Drop` can be a no-op if we need to manually forget
    /// the lease in some future path (not used today). Always `Some`
    /// for leases built via `LeaseTracker::acquire`.
    tracker: Option<Arc<LeaseTracker>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(tracker) = self.tracker.take() {
            let prev = tracker.inflight.fetch_sub(1, Ordering::SeqCst);
            // `prev` was the count *before* decrement. After decrement
            // it's `prev - 1`; if that's zero we just went idle.
            tracker
                .last_release_ms
                .store(now_millis(), Ordering::SeqCst);
            if prev == 1 {
                tracker.idle_notify.notify_one();
            }
        }
    }
}

/// Wall-clock millis since the Unix epoch, truncated to `i64`. We
/// only use this for "has N ms elapsed since X" checks, so clock
/// skew or leap-second weirdness is irrelevant as long as the values
/// are monotonically close on a short horizon. On systems whose
/// clock is ludicrously before 1970, falls back to 0 rather than
/// panicking — the worst case is that Phase B's idle watcher treats
/// the server as "just released" and defers the kill.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Liveness state of the shared `opencode serve` process.
///
/// Full transition diagram:
/// ```text
///   Stopped → Starting      (first ensure_server wins spawn race)
///   Starting → Running      (spawn succeeded; notify waiters)
///   Starting → Stopped      (spawn failed; next caller retries)
///   Running → Draining      (idle watcher decided to kill)
///   Draining → Stopped      (shutdown() returned; notify waiters)
/// ```
///
/// `Draining` blocks new ensure_server callers the same way
/// `Starting` does: they `notified().await` on `server_ready` and
/// re-check once the transition completes. This prevents a caller
/// from racing in, finding a "Running" server that's actually
/// mid-SIGTERM, and sending requests into a dying process.
enum ServerSlot {
    /// No child process; next `ensure_server` will spawn one.
    Stopped,
    /// A caller has claimed the spawn slot and is running the
    /// spawn future. Other callers block on `server_ready` until
    /// the state transitions out of this variant.
    Starting,
    /// Server is live. Holds the child-process handle and the
    /// generation tag for native-id validation. The lease tracker
    /// is adapter-scoped (not per-running-state) so the watcher
    /// can observe leases continuously across generations.
    Running(ServerRunning),
    /// Idle watcher committed to killing this server. The child is
    /// between SIGTERM and reaped; new callers must wait for
    /// `Stopped` before spawning fresh. The Arc kept here is
    /// cosmetic — `shutdown()` runs on a clone held locally in the
    /// watcher; we retain the reference so the SSE reader's
    /// `Weak::upgrade` check sees "still alive" until `Stopped` is
    /// set, giving us one place (the Stopped transition) to reason
    /// about reader teardown.
    Draining(Arc<OpenCodeServer>),
}

/// Bundle of state owned by the `Running` variant.
struct ServerRunning {
    server: Arc<OpenCodeServer>,
    /// Sequence number of this particular server process. Incremented
    /// on every Stopped → Running transition (see
    /// `OpenCodeAdapter::server_generation`). Cached native session
    /// ids tagged with a different generation are stale and must be
    /// discarded before use.
    generation: u64,
}

#[derive(Clone)]
pub struct OpenCodeAdapter {
    /// Working-directory fallback handed to the server process when a
    /// session doesn't carry its own `cwd`. Sessions attached to a
    /// project override this per-request via the REST API's
    /// `directory` parameter; the server's own cwd is only used as a
    /// last-resort default. Also the location where we drop the
    /// shared `opencode.json` — see `write_shared_opencode_json`.
    working_directory: PathBuf,
    /// Liveness state of the shared `opencode serve`. See
    /// [`ServerSlot`] for the transition diagram.
    ///
    /// # Why shared, not per-session
    ///
    /// A prior iteration spawned one `opencode serve` per flowstate
    /// session so each one could load a session-scoped
    /// `opencode.json` with its own `flowstate_session_id` baked into
    /// the MCP command array. That isolated cross-provider
    /// orchestration correctly but added a ~1–5s cold start to every
    /// new session. Shared-server is the current trade: we write one
    /// `opencode.json` with a sentinel session id
    /// ([`OPENCODE_SHARED_SESSION_ID`]) so opencode agents *do* see
    /// the orchestration tools, at the cost of their calls all
    /// appearing to the runtime as coming from the same origin. Per-
    /// session isolation is a Phase-2 follow-up if that cost ever
    /// bites.
    server_slot: Arc<Mutex<ServerSlot>>,
    /// Notifies callers blocked on a concurrent spawn/drain (i.e. the
    /// slot is `Starting` or `Draining`) that the state has advanced
    /// and they can retry the check. Also pinged on spawn-failure so
    /// losers don't wait forever for a success that never lands.
    server_ready: Arc<Notify>,
    /// Incremented each time we move `Stopped → Running`. Sessions
    /// carry this in `provider_state.metadata` as
    /// `{"opencode_generation": N}`; a mismatch means the native id
    /// was minted against a dead server (idle-killed then respawned)
    /// and must be recreated.
    server_generation: Arc<AtomicU64>,
    /// Adapter-scoped lease tracker. Lives across server generations
    /// so the idle watcher task can hold a single `Arc<LeaseTracker>`
    /// and keep watching after each respawn without re-plumbing.
    lease_tracker: Arc<LeaseTracker>,
    /// Idle-kill configuration. `Some(ttl)` with `ttl > 0` spawns an
    /// idle watcher task at construction that tears the server down
    /// after `ttl` of continuous zero-lease time. `None` or
    /// `Some(Duration::ZERO)` disables idle-kill entirely — the
    /// server stays warm until the daemon exits, matching pre–Phase B
    /// behaviour. Read from `UserConfigStore`
    /// (`opencode.idle_ttl_seconds`) at adapter construction; not
    /// hot-reloadable in the current implementation.
    idle_ttl: Option<Duration>,
    /// Shared handle over the runtime's loopback HTTP transport. When
    /// populated at the time the shared server spawns, we drop an
    /// `opencode.json` into `working_directory` that registers the
    /// `flowstate` MCP server. Empty in dev builds that don't mount
    /// the loopback.
    orchestration: Option<OrchestrationIpcHandle>,
    /// Routes SSE events to the right session's sink. Lives at the
    /// adapter level because one SSE connection feeds every in-flight
    /// session — multiplexing happens here, not per-session. Also
    /// survives a respawn: the router's session map is shared across
    /// server generations (subscribers get a synthetic failure via
    /// `fail_all_in_flight` when a kill happens).
    event_router: Arc<EventRouter>,
}

impl OpenCodeAdapter {
    /// Construct without cross-provider orchestration wiring and
    /// with idle-kill **disabled**. Used by headless tests and dev
    /// builds that don't want a background watcher task running.
    /// Production paths (`daemon_main::build_adapters`, Tauri
    /// `setup`) use [`Self::new_with_orchestration_and_idle_ttl`]
    /// with the TTL resolved from [`UserConfigStore`], which
    /// defaults ON at 10 minutes.
    pub fn new(working_directory: PathBuf) -> Self {
        Self::new_with_orchestration_and_idle_ttl(
            working_directory,
            None,
            None,
        )
    }

    /// Construct with an optional [`OrchestrationIpcHandle`]. Uses
    /// [`DEFAULT_IDLE_TTL`] for idle-kill. Prefer
    /// [`Self::new_with_orchestration_and_idle_ttl`] when the host has
    /// a user-config store to read the TTL from.
    pub fn new_with_orchestration(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
    ) -> Self {
        Self::new_with_orchestration_and_idle_ttl(
            working_directory,
            orchestration,
            Some(DEFAULT_IDLE_TTL),
        )
    }

    /// Construct with full control over the idle-kill TTL. `None` or
    /// `Some(Duration::ZERO)` disables idle-kill (server stays warm
    /// for the life of the daemon). Any other `Some(ttl)` spawns an
    /// idle watcher task that tears the server down after `ttl` of
    /// continuous zero-lease time.
    ///
    /// When populated, the first `ensure_server` call writes a shared
    /// `opencode.json` into `working_directory` registering the
    /// flowstate MCP server with a sentinel session id
    /// ([`OPENCODE_SHARED_SESSION_ID`]). Opencode then spawns one
    /// long-lived `flowstate mcp-server` subprocess that serves every
    /// session on this `opencode serve`.
    pub fn new_with_orchestration_and_idle_ttl(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
        idle_ttl: Option<Duration>,
    ) -> Self {
        // Normalise: a zero TTL is equivalent to "disabled" — easier
        // for callers who read `UserConfigStore` values (0 = off is
        // the documented contract there).
        let idle_ttl = idle_ttl.filter(|d| !d.is_zero());

        let adapter = Self {
            working_directory,
            server_slot: Arc::new(Mutex::new(ServerSlot::Stopped)),
            server_ready: Arc::new(Notify::new()),
            server_generation: Arc::new(AtomicU64::new(0)),
            lease_tracker: LeaseTracker::new(),
            idle_ttl,
            orchestration,
            event_router: Arc::new(EventRouter::new()),
        };

        // Fire-and-forget the watcher. It holds `Weak` references so
        // dropping the adapter lets the task self-exit — no explicit
        // shutdown handshake needed.
        if let Some(ttl) = idle_ttl {
            IdleWatcher::spawn(&adapter, ttl);
        }

        adapter
    }

    /// Render + write the single shared `opencode.json` that
    /// registers the flowstate MCP server for every session hosted
    /// on the shared `opencode serve`. Called once, right before the
    /// server spawn — opencode reads the file at startup and holds
    /// the MCP subprocess open for the server's entire lifetime.
    ///
    /// Session id is the sentinel [`OPENCODE_SHARED_SESSION_ID`];
    /// see its docstring for the origin-tracking tradeoffs. Errors
    /// here are non-fatal: if the write fails, the server still
    /// starts, the MCP entry just isn't registered, and opencode
    /// agents see no flowstate tools — equivalent to running without
    /// orchestration wiring at all.
    async fn write_shared_opencode_json(&self, info: &OrchestrationIpcInfo) {
        let path = self.working_directory.join("opencode.json");
        let payload = serde_json::json!({
            "mcp": {
                "flowstate": {
                    "type": "local",
                    "command": [
                        info.executable_path.to_string_lossy(),
                        "mcp-server",
                        "--http-base",
                        &info.base_url,
                        "--session-id",
                        OPENCODE_SHARED_SESSION_ID,
                    ],
                    "environment": {
                        "FLOWSTATE_SESSION_ID": OPENCODE_SHARED_SESSION_ID,
                        "FLOWSTATE_HTTP_BASE": &info.base_url,
                        // Parent watchdog key — see `mcp-server`'s
                        // `spawn_parent_watchdog`. When flowstate dies
                        // the proxy self-exits within ~2 s.
                        "FLOWSTATE_PID": std::process::id().to_string(),
                    }
                }
            }
        });
        let body = match serde_json::to_string_pretty(&payload) {
            Ok(s) => s,
            Err(err) => {
                warn!(%err, "failed to render shared opencode.json; orchestration disabled");
                return;
            }
        };
        if let Err(err) = tokio::fs::write(&path, body).await {
            warn!(
                path = %path.display(),
                %err,
                "failed to write shared opencode.json; orchestration disabled for opencode sessions"
            );
            return;
        }
        debug!(
            path = %path.display(),
            session_id = OPENCODE_SHARED_SESSION_ID,
            "wrote shared opencode.json with flowstate MCP entry"
        );
    }

    /// Resolve the `opencode` binary. Returns `None` if nothing
    /// resembling opencode is on PATH or in the usual install
    /// locations; `health()` turns that into a user-visible
    /// "install opencode" hint rather than a cryptic spawn failure
    /// down the line.
    fn find_opencode_binary() -> Option<String> {
        find_cli_binary(OPENCODE_BINARY).map(|p| p.to_string_lossy().into_owned())
    }

    /// Ensure the server is running and return a handle **plus** a
    /// [`Lease`] representing the caller's in-flight claim on it. The
    /// lease MUST be held for the entire duration of the work that
    /// needs the server alive; releasing early lets Phase B's idle
    /// watcher kill the child mid-operation.
    ///
    /// Concurrency model:
    /// - `Running` → clone the server Arc, mint a lease, return.
    /// - `Starting` → another caller is mid-spawn; await
    ///   `server_ready` and retry.
    /// - `Stopped` → claim the spawn slot (transition to `Starting`),
    ///   release the mutex for the spawn duration, spawn, re-acquire
    ///   the mutex to install the `Running` state, and notify any
    ///   waiters.
    ///
    /// The spawn runs *without* the slot mutex held — holding it for
    /// the multi-second readiness probe would serialise every other
    /// call path (`health()`, `fetch_models()`, `execute_turn()` on
    /// other sessions). The `Starting` state + `server_ready` Notify
    /// take over the serialisation job.
    async fn ensure_server(&self) -> Result<(Arc<OpenCodeServer>, Lease), String> {
        loop {
            let mut slot = self.server_slot.lock().await;
            match &*slot {
                ServerSlot::Running(state) => {
                    let lease = self.lease_tracker.acquire();
                    let server = state.server.clone();
                    return Ok((server, lease));
                }
                ServerSlot::Starting | ServerSlot::Draining(_) => {
                    // Another caller is mid-spawn or the idle watcher
                    // is tearing the child down. Either way we must
                    // wait for the transition to complete before
                    // deciding what to do next. Register the waiter
                    // BEFORE dropping the mutex so we can't miss the
                    // `notify_waiters` that happens after the state
                    // transition.
                    let notified = self.server_ready.notified();
                    drop(slot);
                    notified.await;
                    continue;
                }
                ServerSlot::Stopped => {
                    // Claim the spawn slot.
                    *slot = ServerSlot::Starting;
                    drop(slot);

                    let spawn_result = self.spawn_server().await;

                    let mut slot = self.server_slot.lock().await;
                    match spawn_result {
                        Ok(running) => {
                            let lease = self.lease_tracker.acquire();
                            let server = running.server.clone();
                            *slot = ServerSlot::Running(running);
                            drop(slot);
                            self.server_ready.notify_waiters();
                            return Ok((server, lease));
                        }
                        Err(err) => {
                            // Release the slot so the next caller can
                            // retry the spawn. A persistent failure
                            // will keep bouncing through this branch
                            // — caller-level backoff is up to the
                            // health / runtime layer.
                            *slot = ServerSlot::Stopped;
                            drop(slot);
                            self.server_ready.notify_waiters();
                            return Err(err);
                        }
                    }
                }
            }
        }
    }

    /// Run one spawn attempt. Extracted so `ensure_server` can call
    /// it without holding the slot mutex (the spawn itself can take
    /// seconds and would otherwise block every concurrent caller).
    async fn spawn_server(&self) -> Result<ServerRunning, String> {
        // Drop the shared `opencode.json` into the server's cwd
        // BEFORE spawning — opencode reads the file at startup and
        // holds the resulting MCP subprocess open for the server's
        // lifetime. Writing after would be a no-op (opencode doesn't
        // hot-reload). Skipped when orchestration isn't wired (dev
        // builds, tests); opencode simply runs without the flowstate
        // MCP entry in that case.
        if let Some(info) = self.orchestration.as_ref().and_then(|h| h.get()) {
            self.write_shared_opencode_json(&info).await;
        }

        let binary = Self::find_opencode_binary().ok_or_else(|| {
            format!(
                "opencode binary not found on PATH. {}",
                OPENCODE_INSTALL_HINT
            )
        })?;
        let server = OpenCodeServer::spawn(
            &binary,
            &self.working_directory,
            std::time::Duration::from_secs(SERVER_STARTUP_TIMEOUT_SECS),
        )
        .await?;
        let server = Arc::new(server);

        // Kick the SSE reader once the server is live so the first
        // turn's events land in the router the moment opencode starts
        // streaming. The reader holds a `Weak<OpenCodeServer>` — when
        // Phase B's idle-kill drops the last strong Arc, the reader
        // self-exits on its next iteration (see `read_forever`).
        self.event_router
            .spawn_reader(server.clone(), server.client());

        // Mint the generation AFTER the server is up. `fetch_add(1)`
        // returns the pre-increment value, so the first server gets
        // 0, the next gets 1, etc. After a Phase-B idle-kill, the
        // next successful spawn lands on generation 1, invalidating
        // every cached native id tagged with 0.
        let generation = self.server_generation.fetch_add(1, Ordering::SeqCst);
        Ok(ServerRunning { server, generation })
    }

    /// Convenience: acquire a client + lease bundle. Every caller
    /// that touches the opencode HTTP API must hold the lease for
    /// the entire scope of the work — Phase B's idle watcher
    /// considers a server with zero leases eligible for kill.
    async fn client(&self) -> Result<(Arc<OpenCodeClient>, Lease), String> {
        let (server, lease) = self.ensure_server().await?;
        Ok((server.client(), lease))
    }

    /// Non-spawning variant: returns `Some((client, lease))` only if
    /// the server is already live. Used by paths like
    /// [`Self::interrupt_turn`] where spawning a fresh server just
    /// to send a kill signal is pointless — if the server is down,
    /// the turn that would need interrupting is already gone.
    ///
    /// Phase A: callers still use `client()` for backwards-compat;
    /// this helper is wired in so Phase B can switch without another
    /// plumbing pass.
    async fn try_running(&self) -> Option<(Arc<OpenCodeClient>, Lease)> {
        let slot = self.server_slot.lock().await;
        if let ServerSlot::Running(state) = &*slot {
            let lease = self.lease_tracker.acquire();
            let client = state.server.client();
            Some((client, lease))
        } else {
            None
        }
    }

    /// Current server generation, or `None` if the server isn't
    /// running (Stopped / Starting / Draining). Used by
    /// [`Self::native_id_if_current_generation`] to decide whether a
    /// cached native session id is still valid.
    async fn current_generation(&self) -> Option<u64> {
        let slot = self.server_slot.lock().await;
        if let ServerSlot::Running(state) = &*slot {
            Some(state.generation)
        } else {
            None
        }
    }

    /// Return a cached native session id iff it was minted against
    /// the currently-running server generation. Returns `None` if
    /// the session has no native id, the server isn't running, or
    /// the recorded generation doesn't match — all three cases
    /// resolve to "create a fresh native session".
    ///
    /// Sessions that pre-date the generation tag (no metadata, or
    /// metadata missing `GENERATION_METADATA_KEY`) are treated as
    /// matching the current generation. Rationale: at Phase A
    /// rollout time the server has never respawned, so every
    /// existing session's cached native id is valid. Invalidating
    /// them all would force every opencode session to lose its
    /// conversation history on upgrade, for no correctness benefit.
    async fn native_id_if_current_generation(
        &self,
        session: &SessionDetail,
    ) -> Option<String> {
        let state = session.provider_state.as_ref()?;
        let native = state.native_thread_id.as_deref()?;
        let current = self.current_generation().await?;
        let gen_ok = state
            .metadata
            .as_ref()
            .and_then(|m| m.get(GENERATION_METADATA_KEY))
            .and_then(|v| v.as_u64())
            .map(|g| g == current)
            .unwrap_or(true);
        if gen_ok {
            Some(native.to_string())
        } else {
            debug!(
                session_id = %session.summary.session_id,
                native = %native,
                "opencode: discarding native id from stale server generation"
            );
            None
        }
    }

    /// Build a [`ProviderSessionState`] tagged with the current
    /// server generation. Use this whenever a fresh native id is
    /// minted so that a later Phase-B respawn can detect the
    /// staleness via [`Self::native_id_if_current_generation`].
    fn provider_state_for(native_id: String, generation: u64) -> ProviderSessionState {
        ProviderSessionState {
            native_thread_id: Some(native_id),
            metadata: Some(serde_json::json!({
                GENERATION_METADATA_KEY: generation,
            })),
        }
    }
}

/// Weakly-held handles the idle watcher needs to make a kill
/// decision. `Weak` so the task self-exits when the adapter is
/// dropped (daemon shutdown) without requiring an explicit cancel
/// channel — the same trick used by the SSE reader in `events.rs`.
struct IdleWatcher {
    server_slot: std::sync::Weak<Mutex<ServerSlot>>,
    server_ready: std::sync::Weak<Notify>,
    lease_tracker: std::sync::Weak<LeaseTracker>,
    event_router: std::sync::Weak<EventRouter>,
    ttl: Duration,
}

impl IdleWatcher {
    /// Spawn the watcher task. Called from the adapter constructor
    /// when `idle_ttl` is `Some(non-zero)`. The task runs until every
    /// `Weak` handle fails to upgrade, which happens when the last
    /// `OpenCodeAdapter` clone is dropped.
    fn spawn(adapter: &OpenCodeAdapter, ttl: Duration) {
        let watcher = IdleWatcher {
            server_slot: Arc::downgrade(&adapter.server_slot),
            server_ready: Arc::downgrade(&adapter.server_ready),
            lease_tracker: Arc::downgrade(&adapter.lease_tracker),
            event_router: Arc::downgrade(&adapter.event_router),
            ttl,
        };
        tokio::spawn(async move { watcher.run().await });
    }

    async fn run(self) {
        debug!(ttl_ms = self.ttl.as_millis(), "opencode idle watcher started");
        loop {
            // Upgrade everything we need. If any of these fail the
            // adapter has been dropped and the watcher should exit.
            let Some(tracker) = self.lease_tracker.upgrade() else {
                debug!("opencode idle watcher: adapter dropped; exiting");
                return;
            };

            // Fast path: nothing to watch until leases go to zero.
            // `Notify::notified` registers a waiter first, so we
            // can't miss a `notify_one` that fires after the check.
            let notified = tracker.idle_notify.notified();
            if tracker.inflight.load(Ordering::SeqCst) > 0 {
                notified.await;
            } else {
                // Already idle at entry (or became idle between the
                // upgrade and here). Fall through immediately.
                drop(notified);
            }

            // Compute how much longer we need to wait for the TTL
            // to expire from the *last release*, not from now. This
            // keeps the watcher honest when a short-lived lease
            // cycle (acquire → release → acquire → release) has
            // been pinging us — the relevant idle time is measured
            // from the most recent release.
            let deadline = {
                let last_release = tracker.last_release_ms.load(Ordering::SeqCst);
                let elapsed = (now_millis() - last_release).max(0) as u64;
                self.ttl.as_millis().saturating_sub(elapsed as u128) as u64
            };

            if deadline > 0 {
                // Sleep in granularity-bounded steps so a new lease
                // racing in during the wait can cancel the kill
                // promptly. We re-check inflight and last_release
                // after each step.
                let mut remaining_ms = deadline;
                while remaining_ms > 0 {
                    let step = remaining_ms.min(IDLE_CHECK_GRANULARITY.as_millis() as u64);
                    tokio::time::sleep(Duration::from_millis(step)).await;
                    remaining_ms = remaining_ms.saturating_sub(step);
                    // A lease arrived? Abort the countdown and go
                    // back to waiting for the next idle notification.
                    if tracker.inflight.load(Ordering::SeqCst) > 0 {
                        break;
                    }
                }
                // Bail back to the outer loop if something grabbed
                // a lease during our sleep — we'll be re-notified
                // when it drops.
                if tracker.inflight.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                // Re-check TTL against the current last_release in
                // case a lease cycled (acquire+release) during the
                // sleep, bumping `last_release_ms` forward.
                let since_release =
                    (now_millis() - tracker.last_release_ms.load(Ordering::SeqCst)).max(0) as u128;
                if since_release < self.ttl.as_millis() {
                    continue;
                }
            }

            // TTL has elapsed with no in-flight work. Transition
            // Running → Draining under the slot lock, then drop
            // the lock to do the actual kill (which can take
            // several hundred ms for SIGTERM → wait).
            let Some(slot_arc) = self.server_slot.upgrade() else {
                return;
            };
            let Some(ready) = self.server_ready.upgrade() else {
                return;
            };

            let server_to_shutdown = {
                let mut slot = slot_arc.lock().await;
                match &*slot {
                    ServerSlot::Running(state) => {
                        // Double-check under the lock: a caller could
                        // have grabbed a lease between our last
                        // atomic check and acquiring the mutex. If
                        // they did, skip the kill this round.
                        if tracker.inflight.load(Ordering::SeqCst) > 0 {
                            continue;
                        }
                        let arc = state.server.clone();
                        *slot = ServerSlot::Draining(arc.clone());
                        Some(arc)
                    }
                    _ => None, // Stopped / Starting / already Draining — nothing to do.
                }
            };

            let Some(server) = server_to_shutdown else {
                continue;
            };

            info!(
                ttl_secs = self.ttl.as_secs(),
                "opencode idle watcher: killing shared opencode serve after idle TTL"
            );

            // Fail any in-flight SSE subscribers cleanly so they
            // surface a concrete error instead of hanging on the
            // completion oneshot forever. In practice `inflight == 0`
            // implies no subscriber was mid-turn (an execute_turn
            // holds a lease across wait_for_completion), so this is
            // mostly a defensive sweep.
            if let Some(router) = self.event_router.upgrade() {
                router
                    .fail_all_in_flight("opencode server idle-killed; respawning on next use")
                    .await;
            }

            // SIGTERM → bounded wait → SIGKILL fallback.
            server.shutdown().await;
            drop(server);

            // Transition to Stopped and wake any callers who were
            // blocked on Draining in ensure_server.
            {
                let mut slot = slot_arc.lock().await;
                if let ServerSlot::Draining(_) = &*slot {
                    *slot = ServerSlot::Stopped;
                }
            }
            ready.notify_waiters();
        }
    }
}

#[async_trait]
impl ProviderAdapter for OpenCodeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenCode
    }

    /// Off by default. opencode is opt-in — users who don't have the
    /// binary installed shouldn't see a warning badge in Settings the
    /// first time they open the app. Mirrors how `provider-claude-cli`
    /// and the Codex CLI provider behave.
    fn default_enabled(&self) -> bool {
        false
    }

    async fn health(&self) -> ProviderStatus {
        // IMPORTANT: every ProviderStatus we return below must carry
        // `features: features_for_kind(ProviderKind::OpenCode)`, NOT
        // `ProviderFeatures::default()`. The broadcast
        // `provider_health_updated` event sends this status verbatim
        // to the frontend, and the reducer *replaces* the whole
        // provider entry. Emitting a default (all-false) features
        // payload here causes the UI to lose any capability flag
        // (effort selector, context breakdown, etc.) the moment a
        // post-bootstrap health check lands — bootstrap alone reads
        // from `persistence::get_cached_health`, which re-hydrates
        // features, but subsequent broadcasts do not go through that
        // re-hydration path.
        let label = ProviderKind::OpenCode.label().to_string();
        let binary = match Self::find_opencode_binary() {
            Some(b) => b,
            None => {
                return ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some(format!(
                        "opencode CLI is not installed. {}",
                        OPENCODE_INSTALL_HINT
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                };
            }
        };

        // Best-effort version probe. We run `opencode --version` ourselves
        // instead of going through the shared `probe_cli` helper because
        // `probe_cli` also wants an `auth` subcommand, and opencode's
        // auth story lives inside the running server (providers are
        // configured per-workspace via `opencode auth login`, not
        // globally). The server itself is the real authentication
        // proof, so we try to start it next.
        let version = match tokio::process::Command::new(&binary)
            .arg("--version")
            .output()
            .await
        {
            Ok(out) => zenui_provider_api::helpers::first_non_empty_line(&out.stdout)
                .or_else(|| zenui_provider_api::helpers::first_non_empty_line(&out.stderr)),
            Err(err) => {
                return ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some(format!(
                        "opencode CLI was found at `{binary}` but `--version` failed: {err}. {}",
                        OPENCODE_INSTALL_HINT
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                };
            }
        };

        // Second probe: check if a server is already running; only
        // ever spawn fresh if we don't have idle-kill enabled.
        //
        // Why the split: with idle-kill on, a periodic health poll
        // would defeat idle shutdown — every tick we'd resurrect the
        // server just to confirm it can boot. Instead, when idle-kill
        // is active and the server is Stopped, we report Ready with
        // an "idle, will resume on next turn" message. The binary
        // version probe above already confirmed the install works;
        // full spawn is deferred to actual use. When idle-kill is
        // disabled we preserve the historical behaviour (spawn
        // on-demand from the health probe).
        if self.idle_ttl.is_some() {
            if let Some((client, _lease)) = self.try_running().await {
                // Server already live — do the real health probe.
                match client.health().await {
                    Ok(()) => {
                        return ProviderStatus {
                            kind: ProviderKind::OpenCode,
                            label: label.clone(),
                            installed: true,
                            authenticated: true,
                            version,
                            status: ProviderStatusLevel::Ready,
                            message: Some(format!("{label} server is running.")),
                            models: Vec::new(),
                            enabled: true,
                            features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                        };
                    }
                    Err(err) => {
                        return ProviderStatus {
                            kind: ProviderKind::OpenCode,
                            label,
                            installed: true,
                            authenticated: false,
                            version,
                            status: ProviderStatusLevel::Warning,
                            message: Some(format!(
                                "opencode server responded unhealthily: {err}"
                            )),
                            models: Vec::new(),
                            enabled: true,
                            features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                        };
                    }
                }
            } else {
                // Server is Stopped (idle-killed or never spawned).
                // Report Ready with an informational message — the
                // next user turn will respawn. This is the key line
                // that keeps idle-kill from being defeated by
                // periodic health polls.
                return ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label: label.clone(),
                    installed: true,
                    authenticated: true,
                    version,
                    status: ProviderStatusLevel::Ready,
                    message: Some(format!(
                        "{label} idle; will resume on next use."
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                };
            }
        }

        // Idle-kill disabled: historical path — ensure_server spawns
        // on demand so the first health probe after daemon boot
        // warms the server up.
        let status = match self.ensure_server().await {
            // Lease is held for the duration of the health probe and
            // dropped when the arm falls out of scope. Phase B's idle
            // watcher can kill the server immediately after — that's
            // fine, a subsequent health poll will respawn it (or, in
            // Phase B, be short-circuited to `IdleStopped`).
            Ok((server, _lease)) => match server.client().health().await {
                Ok(()) => ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label: label.clone(),
                    installed: true,
                    authenticated: true,
                    version,
                    status: ProviderStatusLevel::Ready,
                    message: Some(format!("{label} server is running on {}.", server.url())),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                },
                Err(err) => ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: true,
                    authenticated: false,
                    version,
                    status: ProviderStatusLevel::Warning,
                    message: Some(format!(
                        "opencode server started but health probe failed: {err}"
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
                },
            },
            Err(err) => ProviderStatus {
                kind: ProviderKind::OpenCode,
                label,
                installed: true,
                authenticated: false,
                version,
                status: ProviderStatusLevel::Warning,
                message: Some(format!("failed to start opencode server: {err}")),
                models: Vec::new(),
                enabled: true,
                features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
            },
        };

        status
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // `_lease` held across the HTTP round-trip; dropped on return.
        let (client, _lease) = self.client().await?;
        client.list_models().await
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // Acquire the lease up-front — if we need to mint a native
        // id below, the server must stay alive across the HTTP call;
        // and if we reuse an existing id, we still briefly touched
        // the server state via `native_id_if_current_generation`,
        // which is cheap but wants a lease anyway for consistency.
        let (client, _lease) = self.client().await?;

        // If a native session id is already persisted AND it was
        // minted against the currently-running server generation,
        // reuse it — opencode's server keeps the conversation alive
        // across daemon restarts as long as the session row hasn't
        // been deleted. If the generation is stale (Phase B: after a
        // respawn), fall through to the mint path.
        if let Some(existing) = self.native_id_if_current_generation(session).await {
            debug!(
                session_id = %session.summary.session_id,
                native = %existing,
                "opencode: reusing existing native session id"
            );
            return Ok(None);
        }

        let cwd = zenui_provider_api::helpers::session_cwd(session, &self.working_directory);
        // On `start_session` we don't yet know which permission mode
        // the user will run with — the runtime only hands it through
        // on `execute_turn`. Default here to the conservative "ask"
        // ruleset; `execute_turn` recreates the session with the
        // caller's chosen mode if it's different.
        let native_id = client
            .create_session(
                cwd.to_string_lossy().as_ref(),
                session.summary.model.as_deref(),
                PermissionMode::Default,
            )
            .await?;

        // Stamp the current generation into metadata so a future
        // respawn knows this native id belongs to a dead server.
        // `current_generation()` is guaranteed `Some` here because we
        // hold a live lease on the running server — take a best-effort
        // read and fall back to 0 if the server raced to death
        // between the ensure_server and this call (shouldn't happen
        // while the lease is held, but defensive).
        let generation = self.current_generation().await.unwrap_or(0);
        Ok(Some(Self::provider_state_for(native_id, generation)))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        _thinking_mode: Option<ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        if !input.images.is_empty() {
            warn!(
                provider = ?ProviderKind::OpenCode,
                count = input.images.len(),
                "opencode adapter dropping image attachments; multimodal input not yet wired"
            );
        }

        // Lease held across the entire turn — including the up-to-600s
        // `wait_for_completion`. Phase B's idle timer is keyed on
        // "time since last release", not "time since last acquire",
        // so a long turn doesn't starve the watcher: the watcher just
        // can't fire until this lease drops.
        let (client, _lease) = self.client().await?;
        // Snapshot the current server generation so we can stamp it
        // onto the output `ProviderSessionState`. Safe to read once
        // here: the lease keeps the current generation alive until
        // this turn returns.
        let generation = self.current_generation().await.unwrap_or(0);

        // Resolve / create the native opencode session id. Sessions
        // created by a prior `start_session` carry it through
        // `provider_state` AND must be for the current server
        // generation (Phase B: after a respawn, stale ids return
        // `None` here and we mint a fresh one). Sessions created
        // elsewhere (e.g. REPL flows that skip `start_session`) also
        // fall through to the mint path.
        let native_id = match self.native_id_if_current_generation(session).await {
            Some(id) => id,
            None => {
                let cwd = zenui_provider_api::helpers::session_cwd(session, &self.working_directory);
                client
                    .create_session(
                        cwd.to_string_lossy().as_ref(),
                        session.summary.model.as_deref(),
                        permission_mode,
                    )
                    .await?
            }
        };

        // Register this session's sink with the router *before*
        // firing the prompt so we can't miss the first SSE event.
        let subscription = self
            .event_router
            .subscribe(native_id.clone(), events.clone())
            .await;

        let prompt_result = client
            .send_prompt(
                &native_id,
                &input.text,
                session.summary.model.as_deref(),
                reasoning_effort,
                permission_mode,
            )
            .await;
        if let Err(err) = prompt_result {
            subscription.cancel().await;
            return Err(err);
        }

        // Wait for the turn-complete signal emitted by the SSE
        // reader. Returns the accumulated output text on success or
        // a diagnostic error on timeout / server-side failure.
        let output = subscription
            .wait_for_completion(std::time::Duration::from_secs(TURN_TIMEOUT_SECS))
            .await?;

        // `drain_pending` is the contract every adapter has with the
        // sink — orphaned permission / question oneshots must be
        // released when the turn ends, otherwise they leak forever.
        events.drain_pending().await;

        Ok(ProviderTurnOutput {
            output,
            provider_state: Some(Self::provider_state_for(native_id, generation)),
        })
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        else {
            // No native id = no turn in flight from opencode's point of
            // view. Treat as a no-op so the UI's Stop button doesn't
            // surface a scary error.
            return Ok("No opencode session to interrupt.".to_string());
        };

        // Lease held for the abort round-trip.
        let (client, _lease) = self.client().await?;
        client.abort_session(native_id).await?;
        Ok("Interrupt sent to opencode.".to_string())
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        // Tell the router to drop any sink registered for this
        // session, but leave the opencode server alone — other
        // flowstate sessions may still be using it. Server shutdown
        // happens when the `OpenCodeServer` arc is dropped (daemon
        // tear-down) or the child exits on its own.
        if let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        {
            self.event_router.unsubscribe(native_id).await;
        }
        Ok(())
    }

    /// Opencode's MCP subprocess is registered with a sentinel
    /// session id; every orchestration tool call from an opencode
    /// agent arrives on the loopback transport with
    /// `origin.session_id == "opencode-shared"`. The runtime's
    /// dispatcher uses this to route lease acquisition through us
    /// for the duration of the call.
    fn shared_bridge_origin(&self) -> Option<&'static str> {
        Some(OPENCODE_SHARED_SESSION_ID)
    }

    /// Acquire a lease that keeps `opencode serve` alive while an
    /// orchestration tool call from its MCP subprocess is in flight.
    /// Without this hook the idle watcher could fire between two SSE
    /// events — killing the server out from under a pending MCP call
    /// whose response would never arrive.
    ///
    /// Returns `None` only if the server couldn't be brought up
    /// (spawn failure). In that case the dispatch proceeds
    /// lease-less; the MCP subprocess is about to die with its
    /// parent anyway, so there's no correctness benefit to a new
    /// spawn.
    async fn acquire_shared_bridge_lease(&self) -> Option<SharedBridgeGuard> {
        match self.ensure_server().await {
            Ok((_server, lease)) => Some(SharedBridgeGuard::new(lease)),
            Err(err) => {
                debug!(
                    %err,
                    "opencode: failed to acquire shared-bridge lease; dispatch will proceed lease-less"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the adapter-level primitives that Phase B
    //! introduces. These do NOT touch the `OpenCodeServer` child
    //! process — exercising spawn / shutdown / respawn requires the
    //! real `opencode` binary and lives in `tests/live_opencode.rs`
    //! behind `#[ignore]`.
    //!
    //! What's covered here:
    //! - [`LeaseTracker`]: inflight counting, idle_notify firing on
    //!   the transition to zero, last_release_ms update.
    //! - `provider_state_for` / `native_id_if_current_generation`:
    //!   the generation-tag roundtrip that keeps cached native ids
    //!   from being used against a respawned server.
    //! - `OpenCodeAdapter::shared_bridge_origin`: the advertisement
    //!   the runtime uses to route lease acquisition on MCP calls
    //!   from `opencode-shared`.

    use super::*;

    #[tokio::test]
    async fn lease_tracker_counts_inflight() {
        let tracker = LeaseTracker::new();
        assert_eq!(tracker.inflight.load(Ordering::SeqCst), 0);

        let a = tracker.acquire();
        assert_eq!(tracker.inflight.load(Ordering::SeqCst), 1);

        let b = tracker.acquire();
        assert_eq!(tracker.inflight.load(Ordering::SeqCst), 2);

        drop(a);
        assert_eq!(tracker.inflight.load(Ordering::SeqCst), 1);

        drop(b);
        assert_eq!(tracker.inflight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lease_tracker_notifies_on_transition_to_zero() {
        // The idle watcher relies on `notify_one` firing precisely
        // when inflight transitions to zero. Two acquires then two
        // drops should fire exactly one permit (the second drop);
        // the first drop keeps count at 1 and must NOT notify.
        let tracker = LeaseTracker::new();
        let notified = tracker.idle_notify.notified();
        tokio::pin!(notified);

        let a = tracker.acquire();
        let b = tracker.acquire();

        // First drop: count goes 2 -> 1; no notify.
        drop(a);
        // Poll the future briefly — it must NOT be ready.
        let not_yet = tokio::time::timeout(
            std::time::Duration::from_millis(25),
            notified.as_mut(),
        )
        .await;
        assert!(
            not_yet.is_err(),
            "notify fired prematurely on non-zero transition"
        );

        // Second drop: count goes 1 -> 0; notify expected.
        drop(b);
        let fired = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            notified.as_mut(),
        )
        .await;
        assert!(
            fired.is_ok(),
            "notify did NOT fire on transition to zero"
        );
    }

    #[tokio::test]
    async fn lease_tracker_updates_last_release_ms() {
        // `last_release_ms` powers the "idle at least N ms" check
        // in the watcher — a lease that cycles in and out must bump
        // the timestamp each release so the watcher doesn't fire
        // prematurely.
        let tracker = LeaseTracker::new();
        let baseline = tracker.last_release_ms.load(Ordering::SeqCst);

        // Small sleep so the timestamp has a chance to advance.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let lease = tracker.acquire();
        drop(lease);

        let after = tracker.last_release_ms.load(Ordering::SeqCst);
        assert!(
            after >= baseline,
            "last_release_ms should be monotonic non-decreasing"
        );
    }

    #[test]
    fn provider_state_for_tags_generation_metadata() {
        let state = OpenCodeAdapter::provider_state_for("ses_abc".into(), 7);
        assert_eq!(state.native_thread_id.as_deref(), Some("ses_abc"));
        let tag = state
            .metadata
            .as_ref()
            .and_then(|m| m.get(GENERATION_METADATA_KEY))
            .and_then(|v| v.as_u64());
        assert_eq!(tag, Some(7), "generation tag missing from metadata");
    }

    #[tokio::test]
    async fn shared_bridge_origin_advertises_opencode_shared() {
        // Sanity: the runtime's dispatch hook looks up providers by
        // this sentinel. If anyone renames it without updating the
        // dispatcher, orchestration MCP calls from opencode agents
        // will miss the lease path and get killed mid-flight.
        let adapter = OpenCodeAdapter::new_with_orchestration_and_idle_ttl(
            std::env::temp_dir(),
            None,
            None, // idle-kill disabled — don't spawn a watcher we can't tear down cleanly
        );
        assert_eq!(
            adapter.shared_bridge_origin(),
            Some(OPENCODE_SHARED_SESSION_ID)
        );
    }

    #[tokio::test]
    async fn native_id_accepted_when_generation_not_running() {
        // When the server is Stopped, `current_generation()` returns
        // `None`, which forces `native_id_if_current_generation` to
        // return None as well — a fresh native id must be minted at
        // the next ensure_server. This protects against a respawn
        // invalidating every cached id.
        let adapter = OpenCodeAdapter::new_with_orchestration_and_idle_ttl(
            std::env::temp_dir(),
            None,
            None,
        );
        let session = SessionDetail {
            summary: zenui_provider_api::SessionSummary {
                session_id: "s1".into(),
                provider: ProviderKind::OpenCode,
                status: zenui_provider_api::SessionStatus::Ready,
                created_at: "0".into(),
                updated_at: "0".into(),
                turn_count: 0,
                model: None,
                project_id: None,
            },
            turns: vec![],
            provider_state: Some(OpenCodeAdapter::provider_state_for("ses_xyz".into(), 0)),
            cwd: None,
        };

        // Server is Stopped — no current generation, cached id must
        // NOT be returned.
        let resolved = adapter.native_id_if_current_generation(&session).await;
        assert_eq!(resolved, None, "stale id must not be reused when server is down");
    }
}
