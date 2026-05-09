use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::*;

/// A spontaneous model turn that arrived between user-initiated turns.
///
/// Emitted by the bridge when the Claude Code SDK fires a new model
/// iteration with no active user prompt — the most common cause is a
/// background `Bash` task completing and the CLI sending a completion
/// notification. The `RuntimeCore` subscribes to these events via
/// `init_spontaneous_turn_listener` and fires a new `spawn_peer_turn`
/// so the model's response reaches the user without requiring explicit
/// user input.
#[derive(Debug, Clone)]
pub struct SpontaneousTurnEvent {
    /// Flowstate session id of the session that produced this turn.
    pub session_id: String,
    /// The model's full output text for the spontaneous turn.
    pub output: String,
}

/// Opaque RAII guard returned from
/// [`ProviderAdapter::acquire_shared_bridge_lease`]. Holding the guard
/// signals to the provider that its shared bridge (the long-lived
/// process behind which multiple sessions multiplex, e.g. `opencode
/// serve`) has in-flight work and must not be idle-killed. Drop the
/// guard the moment the work finishes; release is idempotent.
///
/// The inner `Box<dyn Any + Send + Sync>` lets each provider stash
/// whatever concrete lease type it wants (an `Arc<LeaseTracker>`
/// decrement guard, a per-request ticket, etc.) without leaking that
/// type into the trait signature.
pub struct SharedBridgeGuard(Box<dyn std::any::Any + Send + Sync>);

impl SharedBridgeGuard {
    /// Wrap a provider-specific lease value. The concrete type is
    /// erased — callers just hold the guard until their work is done
    /// and drop it.
    pub fn new<T: Send + Sync + 'static>(inner: T) -> Self {
        Self(Box::new(inner))
    }
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Whether this provider should be enabled by default when a fresh
    /// user opens the app (no persisted enablement row yet). Override
    /// to `false` for providers that should be opt-in — CLI variants
    /// and experimental adapters typically want this. Persisted user
    /// preferences always take precedence over this default.
    fn default_enabled(&self) -> bool {
        true
    }

    /// Capability flags advertised to the UI. Defaults to the
    /// per-kind table in [`features_for_kind`], but adapters are free
    /// to override — e.g. a future Codex revision that adds tool
    /// heartbeats can flip `tool_progress: true` here without touching
    /// `provider-api`. Keeps the feature shape a per-provider concern
    /// instead of a central match statement that every new capability
    /// has to edit.
    fn features(&self) -> ProviderFeatures {
        features_for_kind(self.kind())
    }

    async fn health(&self) -> ProviderStatus;

    /// Called once by the daemon at startup (and after any respawn
    /// triggered by the Phase 6 supervisor) to let the adapter clean
    /// up state carried over from a prior process. Concrete things
    /// adapters might do here:
    ///
    /// - Clear in-memory session caches that are stale after a
    ///   restart (Claude SDK bridge `query` map, Copilot SDK session
    ///   object refs).
    /// - Reap orphaned subprocess children that somehow survived the
    ///   startup orphan scan (belt and suspenders).
    /// - Reconnect to long-lived upstream services the adapter owns
    ///   (opencode's `opencode serve`; a crashed daemon leaves the
    ///   child running, the startup orphan scan kills it, and
    ///   reconcile_state is where the adapter acknowledges a fresh
    ///   slate is expected).
    ///
    /// Default impl is a no-op so every adapter compiles unchanged.
    /// Adapters override as they gain reconciliation logic; there is
    /// no contract that reconcile must run before `start_session`, so
    /// lazy/just-in-time reconciliation inside `start_session` is
    /// also fine and may be preferred for adapters with many
    /// sessions (avoid a startup-time foreach).
    async fn reconcile_state(&self) -> Result<(), String> {
        Ok(())
    }

    /// Fetch the live model catalog from the upstream CLI / SDK.
    /// Adapters override this; the default returns an empty list (which the
    /// runtime treats as "use the cached or hardcoded fallback").
    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        Ok(Vec::new())
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(None)
    }

    /// Execute a turn. Push streaming events on `events`; return the canonical final output.
    /// The runtime reconciles: if the returned `output` is non-empty it wins; otherwise the
    /// accumulated text from `AssistantTextDelta` events is used.
    ///
    /// # Compaction and conversation history
    ///
    /// Adapters receive `session: &SessionDetail` for convenience — it carries the turn
    /// history, provider state, and session summary. **Adapters must not build or replay
    /// that turn list to their underlying provider.** Each provider manages its own
    /// conversation state natively:
    ///
    /// - **Codex** uses server-side thread state keyed by `provider_state.native_thread_id`,
    ///   reached via `thread/start` / `thread/resume`.
    /// - **Claude Agent SDK** uses the SDK's `resume:` option; zenui persists the SDK
    ///   session id through `provider_state.native_thread_id` so restarts resume cleanly.
    /// - **GitHub Copilot SDK** keeps an in-memory `session` object alive for the bridge's
    ///   lifetime and manages history internally.
    ///
    /// In all three cases, zenui sends **only the new `input` string** plus a resume
    /// reference each turn, and delegates compaction / context-window management to the
    /// provider. `SessionDetail.turns` is consumed by the frontend for chat-history
    /// display but is never replayed to any model.
    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        thinking_mode: Option<ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String>;

    async fn interrupt_turn(&self, _session: &SessionDetail) -> Result<String, String> {
        Ok("Interrupt recorded.".to_string())
    }

    /// Mid-turn permission-mode switch. Adapters that wrap a backend
    /// supporting live mode changes (currently only the Claude Agent SDK
    /// via `query.setPermissionMode`) should forward the request; the
    /// default is a no-op so adapters whose backend takes the mode at
    /// turn-start time silently ignore the request, and the runtime
    /// applies it from the next `execute_turn` instead.
    async fn update_permission_mode(
        &self,
        _session: &SessionDetail,
        _mode: PermissionMode,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Mid-session model switch. Adapters that keep a long-lived bridge
    /// process (the Claude SDK adapter) should forward the new model so
    /// the next turn uses it. Default is a no-op — the runtime persists
    /// the model to the session summary regardless, so adapters that
    /// read it at turn-start time pick it up automatically.
    async fn update_session_model(
        &self,
        _session: &SessionDetail,
        _model: String,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Build a transcript handoff for the **outgoing** adapter when
    /// the user swaps this session onto a different provider. The
    /// returned `Vec<HandoffMessage>` describes the prior conversation
    /// in role/content form so the incoming adapter (or the runtime's
    /// fallback) can seed the new provider's context.
    ///
    /// Default impl walks `session.turns` and emits a `user` then
    /// `assistant` message per completed turn, using the turn's
    /// `input` and `output` fields directly. This is correct for
    /// every adapter today — input/output are the canonical string
    /// pair the runtime already tracks regardless of provider — but
    /// adapters can override to filter (e.g. drop turns with empty
    /// output, omit interrupted turns, etc.) or to flatten
    /// reasoning/tool-use blocks into the assistant content.
    ///
    /// Returning `Err` aborts the swap. Reserve for genuinely
    /// catastrophic states (database corruption, panics in transcript
    /// readers) — a per-turn formatting glitch should yield a
    /// best-effort empty assistant entry, not an error.
    async fn prepare_for_handoff(
        &self,
        session: &SessionDetail,
    ) -> Result<Vec<HandoffMessage>, String> {
        let mut messages = Vec::with_capacity(session.turns.len() * 2);
        for turn in &session.turns {
            // Skip turns that never produced anything (interrupted
            // before first delta) — they'd appear as empty
            // assistant messages and waste context. Same with any
            // turn whose input got swallowed by an early-cancel race.
            if !turn.input.trim().is_empty() {
                messages.push(HandoffMessage {
                    role: "user".to_string(),
                    content: turn.input.clone(),
                });
            }
            if matches!(turn.status, TurnStatus::Completed) && !turn.output.trim().is_empty() {
                messages.push(HandoffMessage {
                    role: "assistant".to_string(),
                    content: turn.output.clone(),
                });
            }
        }
        Ok(messages)
    }

    /// Receive a transcript handoff on the **incoming** adapter. The
    /// adapter has already had `start_session` called when this
    /// fires, so any bridge / native session is in place — this
    /// method's job is to seed that session's conversation log with
    /// the prior turns.
    ///
    /// Returns `Ok(true)` to signal "I consumed the handoff
    /// natively; runtime should NOT inject a context block". Returns
    /// `Ok(false)` to signal "I left the conversation log empty;
    /// runtime should fall back to prepending a textual context
    /// summary to the user's next turn input". Returning `Err`
    /// aborts the swap.
    ///
    /// Default returns `Ok(false)` — every adapter today opts into
    /// the runtime's textual-prepend fallback, which works
    /// regardless of provider transport. Adapters with a native
    /// transcript-replay primitive (e.g. Codex's `thread/start
    /// messages: [...]`, the Claude SDK's input-queue
    /// `shouldQuery: false` push) should override this to do the
    /// native work and return `Ok(true)`.
    async fn accept_handoff(
        &self,
        _session: &SessionDetail,
        _handoff: &[HandoffMessage],
    ) -> Result<bool, String> {
        Ok(false)
    }

    /// Tear down any long-lived resources held for this session (subprocesses, connections).
    async fn end_session(&self, _session: &SessionDetail) -> Result<(), String> {
        Ok(())
    }

    /// Reap the session's live provider subprocess without tearing
    /// down conversation state. The next turn respawns it and the
    /// model resumes (for Claude SDK, via `native_thread_id`).
    ///
    /// Runtime-core calls this after a turn whose stream surfaced a
    /// Claude Code `ScheduleWakeup` tool call, to prevent the CLI's
    /// in-process timer from firing autonomous output into a pipe
    /// nobody is reading (flowstate's bridge-stdout consumer only
    /// runs during an active turn). After invalidation, flowstate's
    /// own persisted wakeup scheduler owns the fire path; the bridge
    /// respawns lazily on the fired user turn.
    ///
    /// Default is a no-op for adapters whose backends don't hold a
    /// per-session subprocess with in-memory timers.
    async fn invalidate_process(&self, _session: &SessionDetail) -> Result<(), String> {
        Ok(())
    }

    /// Inject a user message into the session's *in-flight* turn so
    /// the model sees it on its next iteration without waiting for
    /// the current turn to complete.
    ///
    /// Used by the runtime to surface a peer's `flowstate_send`
    /// payload during a long-running tool such as Claude Code's
    /// `Monitor` (which keeps the SDK Query open across many
    /// sub-iterations without ever crossing a flowstate-visible turn
    /// boundary). Injecting via this path is what closes the "peer
    /// messages stall until the human user types something" bug —
    /// the legacy mailbox-drain alternative only flushes on
    /// `TurnStatus::Completed`, which never fires while Monitor is
    /// active.
    ///
    /// Currently implemented by the Claude SDK adapter — pushes onto
    /// the live `inputQueue` with `shouldQuery: true` and
    /// `priority: 'next'` (added in
    /// `@anthropic-ai/claude-agent-sdk` v0.2.110), so the SDK fires
    /// a turn for this message on its next iteration. Note:
    /// `shouldQuery: false` is *not* enough — per the SDK type
    /// docstring (`sdk.d.ts:3491`), shouldQuery=false messages only
    /// merge into the next querying input, producing a stall equal
    /// to the in-flight tool's poll interval.
    ///
    /// Other adapters inherit the no-op default, which silently
    /// drops the message — the runtime gates calls on
    /// `ProviderFeatures::live_message_injection` so providers
    /// without a real implementation fall back to the mailbox path.
    ///
    /// No-op (Ok) when no live provider session exists yet — the
    /// caller is expected to have triggered at least one
    /// `execute_turn` first; otherwise there's no transcript to
    /// inject into.
    async fn append_user_message(
        &self,
        _session: &SessionDetail,
        _text: String,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Return a per-category breakdown of what's currently filling
    /// the session's context window. Powers the "Context breakdown"
    /// popover on the session's token counter.
    ///
    /// Adapters that don't support context introspection return
    /// `Ok(None)` (the default); the frontend hides the popover
    /// trigger when `ProviderFeatures.context_breakdown` is false.
    /// Returning `Err` surfaces as a user-visible error toast —
    /// reserve for actual failures, not "not implemented".
    async fn get_context_usage(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ContextBreakdown>, String> {
        Ok(None)
    }

    /// Create or update the session's persisted goal. Forwarded by the
    /// runtime when the user submits the goal dialog.
    ///
    /// `objective` is the free-text "what we're trying to do" string.
    /// `token_budget` is an optional hard cap (None = unbounded).
    /// `status` lets the caller pause/resume an existing goal without
    /// changing its objective; `None` means "leave alone on update,
    /// `Active` on create".
    ///
    /// Returns the resulting goal as the provider has it post-call so
    /// the runtime can synthesize a `RuntimeEvent::ThreadGoalUpdated`
    /// without waiting for a separate notification round-trip.
    /// Agent-initiated mid-turn updates still flow through the
    /// `ProviderTurnEvent::ThreadGoalUpdated` notification path.
    ///
    /// Default `Err` so adapters that haven't wired the surface fail
    /// loudly rather than silently drop the user's intent. Runtime
    /// gates calls on `ProviderFeatures.goal_tracking` first, so this
    /// arm is hit only on misuse / wire churn.
    async fn set_goal(
        &self,
        _session: &SessionDetail,
        _objective: String,
        _token_budget: Option<i64>,
        _status: Option<ThreadGoalStatus>,
    ) -> Result<ThreadGoal, String> {
        Err("This provider does not support goal tracking.".to_string())
    }

    /// Clear the session's active goal. Idempotent — clearing when no
    /// goal is active should respond `Ok(())`. Same gating contract as
    /// `set_goal`: runtime checks `ProviderFeatures.goal_tracking`
    /// first, so the default `Err` only fires on misuse.
    async fn clear_goal(&self, _session: &SessionDetail) -> Result<(), String> {
        Err("This provider does not support goal tracking.".to_string())
    }

    /// Per-provider disk directories the default
    /// [`session_command_catalog`] implementation scans for user-authored
    /// `SKILL.md` files.
    ///
    /// Returns `(home_dirs, project_dirs)`:
    /// - `home_dirs` are resolved under the user's home directory as
    ///   `~/<entry>/skills`. e.g. `"`.claude`"` → `~/.claude/skills`.
    /// - `project_dirs` are resolved under the session's cwd.
    ///
    /// Adapters override this when their ecosystem conventionally scans
    /// extra locations (Copilot adds `.claude/skills` + `.agents/skills`
    /// alongside its own `.copilot/skills`). The default returns a
    /// conservative set that matches Claude's convention, which is the
    /// shared baseline across providers.
    ///
    /// [`session_command_catalog`]: ProviderAdapter::session_command_catalog
    fn skill_scan_roots(&self) -> (&'static [&'static str], &'static [&'static str]) {
        (&[".claude"], &[".claude/skills", ".agents/skills"])
    }

    /// Enumerate the slash commands, agents, and MCP servers available
    /// for this session, merging:
    /// - provider-native commands (adapter-specific; the default impl
    ///   contributes none),
    /// - user-authored `SKILL.md` files on disk (scanned under the
    ///   roots from [`skill_scan_roots`]),
    /// - MCP servers known to the provider (adapter-specific).
    ///
    /// Runtime-core calls this on session start, session load, and
    /// explicit `RefreshSessionCommands`, and broadcasts the result via
    /// [`RuntimeEvent::SessionCommandCatalogUpdated`].
    ///
    /// The default implementation runs a pure disk scan so every
    /// adapter picks up user skills without extra work. Adapters with
    /// a programmatic registry (Claude SDK, Copilot SDK, Claude CLI,
    /// Copilot CLI) override this to merge their native built-ins on
    /// top of the disk scan.
    ///
    /// [`skill_scan_roots`]: ProviderAdapter::skill_scan_roots
    async fn session_command_catalog(
        &self,
        session: &SessionDetail,
    ) -> Result<CommandCatalog, String> {
        let (home_dirs, project_dirs) = self.skill_scan_roots();
        let cwd = session.cwd.as_deref().map(std::path::Path::new);
        let roots = skills_disk::scan_roots_for(home_dirs, project_dirs, cwd);
        let commands = skills_disk::scan(&roots, self.kind());
        Ok(CommandCatalog {
            commands,
            agents: Vec::new(),
            mcp_servers: Vec::new(),
        })
    }

    /// If this provider owns a **shared bridge** — a single long-lived
    /// upstream process behind which multiple flowstate sessions
    /// multiplex — return the sentinel origin `session_id` that
    /// orchestration tool calls from that bridge's MCP subprocess
    /// carry. Returns `None` for providers without such a bridge
    /// (one-subprocess-per-session designs).
    ///
    /// The runtime's orchestration dispatch path uses this to identify
    /// which adapter to consult when an MCP call arrives on the
    /// loopback HTTP transport — so the bridge can be kept alive for
    /// the duration of the call even if its per-session leases are
    /// temporarily zero.
    ///
    /// Currently populated only by the opencode adapter (sentinel
    /// `"opencode-shared"`). Default `None` for everyone else.
    fn shared_bridge_origin(&self) -> Option<&'static str> {
        None
    }

    /// Acquire a lease that keeps this provider's shared bridge alive
    /// for the duration the returned [`SharedBridgeGuard`] is held.
    /// Called by the orchestration dispatcher when an MCP call's
    /// origin session id matches [`Self::shared_bridge_origin`].
    ///
    /// Returns `Some(guard)` on success. `None` means either this
    /// provider has no shared bridge, or the bridge is currently
    /// down and could not be brought up — the caller should proceed
    /// without a lease (the call may be from a stale subprocess that
    /// is about to be reaped; no point in spawning a fresh bridge
    /// just to service it).
    async fn acquire_shared_bridge_lease(&self) -> Option<SharedBridgeGuard> {
        None
    }

    /// Stop all owned subprocesses and background tasks. Called
    /// exactly once by [`graceful_shutdown`] when the daemon is
    /// terminating.
    ///
    /// The daemon's shutdown path previously relied on `Drop` chains
    /// firing at end-of-scope to clean up child processes (e.g.
    /// `OpenCodeServer::Drop` sending `killpg(SIGTERM)`). That path
    /// is racy in two ways:
    ///
    /// 1. Any lingering `Arc` (in transport managed state, background
    ///    tasks, etc.) keeps the adapter alive past scope end, so
    ///    `Drop` never fires.
    /// 2. Rust `Drop` is synchronous — async teardown work (awaiting
    ///    `child.wait()` with a timeout, escalating SIGTERM → SIGKILL)
    ///    cannot run from there.
    ///
    /// `shutdown` sidesteps both by being async and explicit. The
    /// caller in `daemon-core/src/shutdown.rs` iterates adapters and
    /// awaits this method with a per-adapter timeout.
    ///
    /// **Contract for implementors:**
    /// - Best-effort. Log failures; never propagate errors — one
    ///   wedged adapter must not block shutdown of its siblings.
    /// - Bounded. Internal awaits must have timeouts; the caller also
    ///   wraps this whole call in an outer `tokio::time::timeout`.
    /// - Idempotent. Calling twice is legal and must be a no-op the
    ///   second time.
    ///
    /// Default is a no-op — adapters that own no persistent state can
    /// skip overriding.
    async fn shutdown(&self) {}

    /// Run the per-provider CLI upgrade flow (e.g. `npm install -g
    /// @anthropic-ai/claude-code@latest` for the Claude CLI;
    /// `npm install -g @github/copilot@latest` for the Copilot CLI).
    /// Called in response to the user clicking "Upgrade" in the
    /// Settings provider row.
    ///
    /// The default impl returns a friendly "no upgrade flow" message
    /// so adapters that have no native upgrade path (the embedded
    /// Claude SDK bridge, the SaaS Copilot adapter) inherit a sensible
    /// default. Adapters with a real upgrade story override this and
    /// shell out.
    ///
    /// Implementations should be idempotent and safe to retry: an
    /// upgrade that's already up-to-date should still return `Ok`
    /// with a clear message. Errors are surfaced as a toast in the
    /// frontend.
    async fn upgrade(&self) -> Result<String, String> {
        Err(format!(
            "{} has no in-app upgrade flow. Update it through your usual package manager.",
            self.kind().label()
        ))
    }

    /// Return a one-shot receiver for spontaneous model turns that arrive
    /// between user-initiated turns.
    ///
    /// Called at most once by `RuntimeCore::init_spontaneous_turn_listener`
    /// during daemon bootstrap (after `install_self_ref`). The runtime
    /// spawns a background task that reads from the receiver and calls
    /// `spawn_peer_turn` for each event.
    ///
    /// Default: `None` — adapters that don't support between-turn
    /// notifications inherit a no-op. Currently only the Claude SDK
    /// adapter overrides this (via the bridge's persistent background
    /// stdout reader).
    fn take_spontaneous_turn_receiver(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<SpontaneousTurnEvent>> {
        None
    }
}
