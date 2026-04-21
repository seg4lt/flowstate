use async_trait::async_trait;

use crate::*;

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
}
