// Flowstate-app-owned usage analytics store.
//
// Aggregates per-turn token/cost telemetry for the in-app Usage
// dashboard. Lives in its own SQLite file under the app data dir
// (`<app_data_dir>/usage.sqlite`) — deliberately separate from both
// the agent SDK's persistence layer and the app's `user_config.sqlite`.
//
// Why not the SDK's database: usage analytics is pure display-state.
// Nothing in `runtime-core` / `orchestration` / `daemon-core` / a
// provider adapter reads historical usage to make a decision. Per
// the SDK's `persistence/CLAUDE.md` boundary rule, display-only data
// belongs in the consuming app's own store, not the SDK's.
//
// Why a separate file from `user_config.sqlite`: usage rows grow
// per-turn (hundreds per day for power users) while user_config is
// tiny and read on every frontend boot. Keeping them in separate
// files means write-heavy usage recording never contends with the
// hot-path config reads, and users who want to reset their stats
// can delete one file without losing their theme / pool-size /
// worktree settings.
//
// The recorder subscribes to `RuntimeEvent::TurnCompleted` off the
// `RuntimeCore::subscribe()` broadcast in `lib.rs` and calls
// `record_turn` from that task. `turn_id` is the primary key with
// `INSERT OR IGNORE` semantics, so crash-replay / re-emitted
// TurnCompleted events never double-count a turn.
//
// Tables in this file:
//   * `usage_events`        — one row per finalized turn. All the
//                             detail we need to slice ad-hoc.
//   * `usage_daily_rollups` — pre-aggregated by (day_utc, provider,
//                             model). Updated in the same transaction
//                             as the event insert so the rollup is
//                             never stale.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use zenui_provider_api::{
    AgentUsage, ProviderKind, SessionSummary, TokenUsage, TurnRecord, TurnStatus,
};

/// Requested time range for dashboard queries. Resolved to an
/// inclusive `[from, to]` pair in UTC before hitting SQL.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageRange {
    Last7Days,
    Last30Days,
    Last90Days,
    AllTime,
}

impl UsageRange {
    /// Returns `(start_day_utc, end_day_utc)` inclusive, both as
    /// `YYYY-MM-DD` strings. For `AllTime` the start is a sentinel
    /// older than any conceivable flowstate install.
    fn to_day_bounds(self, now: DateTime<Utc>) -> (String, String) {
        let end = now.format("%Y-%m-%d").to_string();
        let start = match self {
            UsageRange::Last7Days => (now - Duration::days(6)).format("%Y-%m-%d").to_string(),
            UsageRange::Last30Days => (now - Duration::days(29)).format("%Y-%m-%d").to_string(),
            UsageRange::Last90Days => (now - Duration::days(89)).format("%Y-%m-%d").to_string(),
            UsageRange::AllTime => "0000-01-01".to_string(),
        };
        (start, end)
    }
}

/// Axis of the dashboard's breakdown. Passed through to SQL
/// `GROUP BY` verbatim (via a whitelist; the JSON tag is never
/// interpolated into a query).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    ByProvider,
    ByModel,
}

impl Default for UsageGroupBy {
    fn default() -> Self {
        Self::ByProvider
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBucket {
    Daily,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub turn_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_cost_usd: f64,
    pub cost_has_unknowns: bool,
    pub total_duration_ms: u64,
    pub distinct_sessions: u64,
    pub distinct_models: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageGroupRow {
    /// Stable key for this group (provider string, model string, or "").
    pub key: String,
    /// Human-readable label. For providers uses `ProviderKind::label`;
    /// for models this is the raw model id and the frontend can
    /// shorten further if it wants.
    pub label: String,
    pub turn_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_cost_usd: f64,
    pub cost_has_unknowns: bool,
    pub total_duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummaryPayload {
    pub range: UsageRange,
    pub totals: UsageTotals,
    pub by_provider: Vec<UsageGroupRow>,
    pub groups: Vec<UsageGroupRow>,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTimeseriesPoint {
    /// Bucket start as a `YYYY-MM-DD` day string. UTC.
    pub bucket_start: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSeries {
    pub key: String,
    pub label: String,
    pub points: Vec<UsageTimeseriesPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTimeseriesPayload {
    pub range: UsageRange,
    pub bucket: UsageBucket,
    /// Zero-filled "total" line over the range.
    pub points: Vec<UsageTimeseriesPoint>,
    /// Per-key series when `split_by` is set; empty otherwise.
    pub series: Vec<UsageSeries>,
    pub generated_at: String,
}

/// One row of the "By agent" dashboard breakdown. Analogous to
/// `UsageGroupRow` but grouped on `agent_type` (with the synthetic
/// "main" key for the parent agent), so power users can see what
/// share of their cost is going to which subagent role. Cost is the
/// proportionally-allocated slice from `usage_event_agents.cost_usd`
/// — it's already computed at insert time, so the GROUP BY here is a
/// plain SUM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAgentGroupRow {
    /// Stable key: `"main"` for the parent agent, catalog subagent
    /// type (e.g. `"Explore"`, `"general-purpose"`) otherwise. The
    /// key is what the dashboard persists in UI state so the user's
    /// preferred sort survives range changes.
    pub key: String,
    pub label: String,
    /// Count of distinct turns that contributed to this bucket.
    /// A single turn that dispatched two Explore subagents
    /// contributes once to `main` and once to `Explore` (turn_count
    /// = 1 in both), matching how the provider/model breakdowns
    /// count turns.
    pub turn_count: u64,
    /// Count of individual invocations: for `main` this equals
    /// `turn_count` (every turn has exactly one parent row); for a
    /// subagent bucket a turn with two dispatches contributes 2. Gives
    /// the dashboard a second column "how many times did this agent
    /// actually run" distinct from "how many turns involved it".
    pub invocation_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_cost_usd: f64,
    pub cost_has_unknowns: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAgentPayload {
    pub range: UsageRange,
    pub groups: Vec<UsageAgentGroupRow>,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopSessionRow {
    pub session_id: String,
    pub provider: String,
    pub provider_label: String,
    pub model: Option<String>,
    pub project_id: Option<String>,
    pub turn_count: u64,
    pub total_cost_usd: f64,
    pub cost_has_unknowns: bool,
    pub last_activity_at: String,
}

/// Per-agent slice of a turn's usage — one row per agent that did
/// work on the turn (main + each Task/Agent sub-agent). Owned by the
/// `UsageEvent` it's attached to; `record_turn` writes these into
/// `usage_event_agents` in the same transaction as the parent event.
#[derive(Debug, Clone)]
pub struct UsageAgentSlice {
    /// `None` identifies the main (parent) agent bucket. Sub-agents
    /// carry the SDK's tool-use id that spawned them.
    pub agent_id: Option<String>,
    /// Catalog type ("Explore", "Plan", "general-purpose", ...). `None`
    /// for main; the dashboard renders main as the synthetic "main"
    /// row when grouping by agent type.
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Raw event mapping — what the subscriber task hands to `record_turn`.
/// Extracted from the incoming `RuntimeEvent::TurnCompleted` so the
/// store's public API is free of provider-api types (keeps the test
/// surface small).
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub turn_id: String,
    pub session_id: String,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub project_id: Option<String>,
    pub status: TurnStatus,
    pub occurred_at: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_cost_usd: f64,
    pub has_cost: bool,
    pub duration_ms: u64,
    /// Per-agent breakdown when the provider supplied one. Empty for
    /// providers that don't attribute tokens per agent; consumers
    /// treat the whole turn as one implicit main-agent bucket in that
    /// case (recovered on read via the COALESCE path in
    /// `summary_by_agent`).
    pub agents: Vec<UsageAgentSlice>,
}

impl UsageEvent {
    /// Derive a `UsageEvent` from a finalized turn + the session it
    /// belongs to. Called from the `RuntimeEvent::TurnCompleted`
    /// subscriber in `lib.rs`.
    pub fn from_turn(session: &SessionSummary, turn: &TurnRecord) -> Self {
        let usage: Option<&TokenUsage> = turn.usage.as_ref();
        let has_cost = usage.map(|u| u.total_cost_usd.is_some()).unwrap_or(false);
        // Prefer the model the provider actually reported on this
        // turn (usage.model) over the session-level fallback — a
        // session can route multiple turns through different models
        // if the user switches mid-thread.
        let model = usage
            .and_then(|u| u.model.clone())
            .or_else(|| session.model.clone());
        // Lift the provider's per-agent breakdown into the store's
        // own type. Kept as an owned Vec (not Option) so downstream
        // code doesn't have to branch — "provider didn't report any
        // agents" is represented by an empty vec, which the insert
        // path then treats as "the whole turn is one implicit main
        // bucket" (matching how the dashboard would render it).
        let agents: Vec<UsageAgentSlice> = usage
            .and_then(|u| u.agents.as_ref())
            .map(|xs| xs.iter().map(agent_slice_from_api).collect())
            .unwrap_or_default();
        Self {
            turn_id: turn.turn_id.clone(),
            session_id: session.session_id.clone(),
            provider: session.provider,
            model,
            project_id: session.project_id.clone(),
            status: turn.status,
            occurred_at: turn.updated_at.clone(),
            input_tokens: usage.map(|u| u.input_tokens).unwrap_or(0),
            output_tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
            cache_read_tokens: usage.and_then(|u| u.cache_read_tokens).unwrap_or(0),
            cache_write_tokens: usage.and_then(|u| u.cache_write_tokens).unwrap_or(0),
            total_cost_usd: usage.and_then(|u| u.total_cost_usd).unwrap_or(0.0),
            has_cost,
            duration_ms: usage.and_then(|u| u.duration_ms).unwrap_or(0),
            agents,
        }
    }
}

/// Project `AgentUsage` (the provider-api wire type) onto `UsageAgentSlice`
/// (the store's owned type). Copies every numeric field verbatim and
/// clones the identifying strings. Kept as a free function so the
/// provider-api type doesn't leak into the store's public surface.
fn agent_slice_from_api(a: &AgentUsage) -> UsageAgentSlice {
    UsageAgentSlice {
        agent_id: a.agent_id.clone(),
        agent_type: a.agent_type.clone(),
        model: a.model.clone(),
        input_tokens: a.input_tokens,
        output_tokens: a.output_tokens,
        cache_read_tokens: a.cache_read_tokens,
        cache_write_tokens: a.cache_write_tokens,
    }
}

/// Owned by Tauri state. The connection is wrapped in a Mutex
/// because rusqlite's Connection is not Sync. Follows the exact
/// pattern of `UserConfigStore`.
pub struct UsageStore {
    connection: Mutex<Connection>,
}

impl UsageStore {
    /// Open (or create) the SQLite file at `<data_dir>/usage.sqlite`
    /// and ensure the schema exists. Called once during Tauri
    /// `setup`. Failing here is *not* fatal — the rest of the app
    /// must continue to work even if analytics storage is broken
    /// (corrupt file, read-only disk, etc.). Callers should log and
    /// fall back to a no-op mode.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return Err(format!("create data dir: {e}"));
        }
        let db_path = data_dir.join("usage.sqlite");
        let connection =
            Connection::open(&db_path).map_err(|e| format!("open usage sqlite: {e}"))?;
        Self::init_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// In-memory store for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, String> {
        let connection =
            Connection::open_in_memory().map_err(|e| format!("open in-memory sqlite: {e}"))?;
        Self::init_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn init_schema(connection: &Connection) -> Result<(), String> {
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS usage_events (
                    turn_id            TEXT PRIMARY KEY,
                    session_id         TEXT NOT NULL,
                    provider           TEXT NOT NULL,
                    model              TEXT,
                    project_id         TEXT,
                    status             TEXT NOT NULL,
                    occurred_at        TEXT NOT NULL,
                    occurred_day_utc   TEXT NOT NULL,
                    input_tokens       INTEGER NOT NULL DEFAULT 0,
                    output_tokens     INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    total_cost_usd     REAL NOT NULL DEFAULT 0.0,
                    has_cost           INTEGER NOT NULL DEFAULT 0,
                    duration_ms        INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_usage_events_day
                    ON usage_events(occurred_day_utc);
                CREATE INDEX IF NOT EXISTS idx_usage_events_provider
                    ON usage_events(provider, occurred_day_utc);
                CREATE INDEX IF NOT EXISTS idx_usage_events_model
                    ON usage_events(model, occurred_day_utc);
                CREATE INDEX IF NOT EXISTS idx_usage_events_session
                    ON usage_events(session_id, occurred_at);

                CREATE TABLE IF NOT EXISTS usage_daily_rollups (
                    day_utc            TEXT NOT NULL,
                    provider           TEXT NOT NULL,
                    model              TEXT NOT NULL DEFAULT '',
                    turn_count         INTEGER NOT NULL,
                    input_tokens       INTEGER NOT NULL,
                    output_tokens      INTEGER NOT NULL,
                    cache_read_tokens  INTEGER NOT NULL,
                    cache_write_tokens INTEGER NOT NULL,
                    total_cost_usd     REAL NOT NULL,
                    total_duration_ms  INTEGER NOT NULL,
                    has_unknown_cost   INTEGER NOT NULL DEFAULT 0,
                    updated_at         TEXT NOT NULL,
                    PRIMARY KEY (day_utc, provider, model)
                );

                CREATE INDEX IF NOT EXISTS idx_usage_daily_rollups_day
                    ON usage_daily_rollups(day_utc);

                CREATE TABLE IF NOT EXISTS usage_event_agents (
                    -- Composite PK with `turn_id` matches the parent
                    -- row's PK so `INSERT OR IGNORE` semantics cascade
                    -- on re-emission of the same turn (matches the
                    -- idempotency contract on `usage_events`).
                    turn_id            TEXT NOT NULL,
                    -- 0 = main (parent) agent, 1..N = sub-agents in
                    -- dispatch order. Used as a tiebreaker in the PK
                    -- and for deterministic sort on display.
                    agent_ordinal      INTEGER NOT NULL,
                    -- NULL for main (the parent agent has no tool-use
                    -- id that spawned it); sub-agents carry the SDK's
                    -- Task/Agent call_id.
                    agent_id           TEXT,
                    -- NULL for main; 'general-purpose'/'Explore'/...
                    -- for sub-agents. Used as the GROUP BY key in
                    -- `summary_by_agent` so cross-turn analytics
                    -- aggregate by agent role rather than by the
                    -- per-turn tool_use id (which is unique per
                    -- dispatch and meaningless across sessions).
                    agent_type         TEXT,
                    model              TEXT,
                    input_tokens       INTEGER NOT NULL DEFAULT 0,
                    output_tokens      INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    -- Cost allocated proportionally from the parent
                    -- turn's `total_cost_usd` using (input + output
                    -- + cache_read + cache_write) as the weight.
                    -- Written at insert time so the dashboard can
                    -- SUM cheaply; the invariant
                    --   SUM(cost_usd) FOR turn_id = usage_events.total_cost_usd
                    -- holds modulo floating-point rounding.
                    cost_usd           REAL NOT NULL DEFAULT 0.0,
                    -- Same semantics as `usage_events.has_cost`: 0
                    -- means the turn-level cost was unknown, which
                    -- propagates to every agent row for that turn.
                    has_cost           INTEGER NOT NULL DEFAULT 0,
                    -- Denormalised for index-covered range queries
                    -- without a JOIN back to `usage_events`.
                    occurred_day_utc   TEXT NOT NULL,
                    provider           TEXT NOT NULL,
                    session_id         TEXT NOT NULL,
                    PRIMARY KEY (turn_id, agent_ordinal)
                );

                CREATE INDEX IF NOT EXISTS idx_usage_event_agents_day
                    ON usage_event_agents(occurred_day_utc);
                CREATE INDEX IF NOT EXISTS idx_usage_event_agents_type
                    ON usage_event_agents(agent_type, occurred_day_utc);",
            )
            .map_err(|e| format!("create usage schema: {e}"))
    }

    /// Record a finalized turn. Idempotent on `turn_id`: a double
    /// emission (crash-replay, lag snapshot) is a no-op for both
    /// tables. The event insert and the rollup upsert share a
    /// transaction so the rollup is never out of sync with the
    /// events.
    pub fn record_turn(&self, event: &UsageEvent) -> Result<(), String> {
        let mut connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let day = day_from_rfc3339(&event.occurred_at);
        let provider_str = event.provider.as_tag();
        let status_str = turn_status_to_str(event.status);
        let model_for_pk = event.model.clone().unwrap_or_default();
        let now = Utc::now().to_rfc3339();

        let tx = connection
            .transaction()
            .map_err(|e| format!("begin usage tx: {e}"))?;

        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO usage_events (
                    turn_id, session_id, provider, model, project_id,
                    status, occurred_at, occurred_day_utc,
                    input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens,
                    total_cost_usd, has_cost, duration_ms
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5,
                    ?6, ?7, ?8,
                    ?9, ?10,
                    ?11, ?12,
                    ?13, ?14, ?15
                 )",
                params![
                    event.turn_id,
                    event.session_id,
                    provider_str,
                    event.model,
                    event.project_id,
                    status_str,
                    event.occurred_at,
                    day,
                    event.input_tokens as i64,
                    event.output_tokens as i64,
                    event.cache_read_tokens as i64,
                    event.cache_write_tokens as i64,
                    event.total_cost_usd,
                    if event.has_cost { 1i64 } else { 0i64 },
                    event.duration_ms as i64,
                ],
            )
            .map_err(|e| format!("insert usage_event: {e}"))?;

        // Per-agent slice rows. Written inside the same transaction
        // so the invariant "sum of per-agent rows = parent event" is
        // atomic — a crash between inserts would be rolled back.
        // Uses `INSERT OR IGNORE` on the composite PK
        // (turn_id, agent_ordinal) so turn replays (crash-replay,
        // lagged broadcast) don't double-insert, matching the parent
        // `usage_events.turn_id` primary key semantics. Only fires
        // when the parent insert was new — for duplicate turns we
        // skip this entire block so we don't even touch the agents
        // table with potentially-different-shaped agents from a
        // retried/interrupted turn (the source of truth is the first
        // insert that won).
        if inserted > 0 {
            insert_agent_rows(&tx, event, &day, provider_str)?;
        }

        // Only roll up if the event was actually new. Duplicate
        // turn_ids (replay) fall through without touching the rollup.
        if inserted > 0 {
            // `has_unknown_cost` is ORed across events: if any
            // contributing row lacked a provider-reported cost, the
            // aggregate is marked partial.
            let flag = if event.has_cost { 0i64 } else { 1i64 };
            tx.execute(
                "INSERT INTO usage_daily_rollups (
                    day_utc, provider, model,
                    turn_count, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens,
                    total_cost_usd, total_duration_ms,
                    has_unknown_cost, updated_at
                 ) VALUES (
                    ?1, ?2, ?3,
                    1, ?4, ?5,
                    ?6, ?7,
                    ?8, ?9,
                    ?10, ?11
                 )
                 ON CONFLICT(day_utc, provider, model) DO UPDATE SET
                    turn_count         = turn_count         + 1,
                    input_tokens       = input_tokens       + excluded.input_tokens,
                    output_tokens      = output_tokens      + excluded.output_tokens,
                    cache_read_tokens  = cache_read_tokens  + excluded.cache_read_tokens,
                    cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens,
                    total_cost_usd     = total_cost_usd     + excluded.total_cost_usd,
                    total_duration_ms  = total_duration_ms  + excluded.total_duration_ms,
                    has_unknown_cost   = MAX(has_unknown_cost, excluded.has_unknown_cost),
                    updated_at         = excluded.updated_at",
                params![
                    day,
                    provider_str,
                    model_for_pk,
                    event.input_tokens as i64,
                    event.output_tokens as i64,
                    event.cache_read_tokens as i64,
                    event.cache_write_tokens as i64,
                    event.total_cost_usd,
                    event.duration_ms as i64,
                    flag,
                    now,
                ],
            )
            .map_err(|e| format!("upsert usage_daily_rollups: {e}"))?;
        }

        tx.commit().map_err(|e| format!("commit usage tx: {e}"))?;
        Ok(())
    }

    /// Aggregate totals + per-provider + caller-requested breakdown
    /// for a range. Queries `usage_events` directly (not the rollup)
    /// so distinct-session / distinct-model counts are cheap.
    pub fn summary(
        &self,
        range: UsageRange,
        group_by: UsageGroupBy,
    ) -> Result<UsageSummaryPayload, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Utc::now();
        let (from, to) = range.to_day_bounds(now);

        let totals = read_totals(&connection, &from, &to)?;
        let by_provider = read_group(&connection, &from, &to, UsageGroupBy::ByProvider)?;
        let groups = if group_by == UsageGroupBy::ByProvider {
            by_provider.clone()
        } else {
            read_group(&connection, &from, &to, group_by)?
        };

        Ok(UsageSummaryPayload {
            range,
            totals,
            by_provider,
            groups,
            generated_at: now.to_rfc3339(),
        })
    }

    /// Daily time series over the range, zero-filled so the chart
    /// x-axis has no gaps. When `split_by` is set, also returns one
    /// `UsageSeries` per key (provider or model) stacked on the same
    /// zero-filled day axis.
    pub fn timeseries(
        &self,
        range: UsageRange,
        bucket: UsageBucket,
        split_by: Option<UsageGroupBy>,
    ) -> Result<UsageTimeseriesPayload, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Utc::now();
        let (from, to) = range.to_day_bounds(now);

        // For AllTime we use the earliest event day as the start of
        // the zero-fill axis, falling back to `to` when there's no
        // data yet.
        let axis_from = if matches!(range, UsageRange::AllTime) {
            earliest_event_day(&connection)?.unwrap_or_else(|| to.clone())
        } else {
            from.clone()
        };

        let days = day_axis(&axis_from, &to);
        let points = read_daily_points(&connection, &from, &to, &days)?;

        let series = match split_by {
            None => Vec::new(),
            Some(split) => read_daily_series(&connection, &from, &to, split, &days)?,
        };

        Ok(UsageTimeseriesPayload {
            range,
            bucket,
            points,
            series,
            generated_at: now.to_rfc3339(),
        })
    }

    /// Aggregate token/cost totals broken down by agent role over the
    /// range. Rolls the `usage_event_agents` table up by `agent_type`
    /// (with `NULL` collapsed to the synthetic `"main"` key), sorted
    /// by cost descending so the biggest spender is always the first
    /// row the dashboard renders.
    ///
    /// Counts two different things side-by-side on every row:
    ///   * `turn_count` — turns where this agent role did ANY work.
    ///     A turn that dispatched three Explore subagents counts once
    ///     in the Explore row (matches the semantics of provider /
    ///     model groupings elsewhere on the dashboard).
    ///   * `invocation_count` — individual agent invocations. Same
    ///     turn contributes 3 to Explore's invocation_count. This
    ///     gives the dashboard a cheap "Explore ran 17 times across
    ///     9 turns" summary without a second query.
    pub fn summary_by_agent(&self, range: UsageRange) -> Result<UsageAgentPayload, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Utc::now();
        let (from, to) = range.to_day_bounds(now);

        // Use COALESCE(agent_type, 'main') both as the GROUP BY key
        // and as the returned key so the main bucket has a stable
        // name. The CASE on has_cost propagates the "one row had
        // unknown cost" marker up to the aggregate level.
        let mut stmt = connection
            .prepare(
                "SELECT COALESCE(agent_type, 'main') AS agent_key,
                        COUNT(DISTINCT turn_id)      AS turn_count,
                        COUNT(*)                     AS invocation_count,
                        COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_read_tokens), 0),
                        COALESCE(SUM(cache_write_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0),
                        MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
                 FROM usage_event_agents
                 WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
                 GROUP BY agent_key
                 ORDER BY SUM(cost_usd) DESC, COUNT(*) DESC",
            )
            .map_err(|e| format!("prepare by_agent: {e}"))?;

        let rows = stmt
            .query_map(params![from, to], |row| {
                let key: String = row.get(0)?;
                // Friendly label: expose `main` as "Main agent"; keep
                // subagent labels verbatim (they already come from the
                // provider's catalog and are human-readable).
                let label = if key == "main" {
                    "Main agent".to_string()
                } else {
                    key.clone()
                };
                Ok(UsageAgentGroupRow {
                    key,
                    label,
                    turn_count: row.get::<_, i64>(1)? as u64,
                    invocation_count: row.get::<_, i64>(2)? as u64,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                    cache_write_tokens: row.get::<_, i64>(6)? as u64,
                    total_cost_usd: row.get(7)?,
                    cost_has_unknowns: row.get::<_, i64>(8)? != 0,
                })
            })
            .map_err(|e| format!("query by_agent: {e}"))?;
        let mut groups = Vec::new();
        for r in rows {
            groups.push(r.map_err(|e| format!("row by_agent: {e}"))?);
        }
        Ok(UsageAgentPayload {
            range,
            groups,
            generated_at: now.to_rfc3339(),
        })
    }

    /// Two-row rollup of `usage_event_agents` over the range: one row
    /// for the main (parent) agent, one row aggregating every subagent
    /// invocation. Lets the dashboard answer "how much of my spend is
    /// actually going to Task/Agent dispatches?" at a glance without
    /// the user having to eyeball the per-type table.
    ///
    /// Semantics match `summary_by_agent`:
    ///   * `turn_count` counts distinct turns where the bucket did any
    ///     work. A turn that dispatched three Explore subagents
    ///     contributes 1 to BOTH `main` (the parent ran) and
    ///     `subagent` (at least one subagent ran).
    ///   * `invocation_count` counts individual agent invocations. The
    ///     same three-Explore turn contributes 1 to `main` and 3 to
    ///     `subagent`.
    ///
    /// Returns the same payload type as `summary_by_agent` so the
    /// frontend can share types; `groups` is just always ≤ 2 rows
    /// (missing when the range has no activity of that kind).
    pub fn summary_by_agent_role(&self, range: UsageRange) -> Result<UsageAgentPayload, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Utc::now();
        let (from, to) = range.to_day_bounds(now);

        // Stable 2-key GROUP BY: `agent_type IS NULL` identifies the
        // main bucket (see `insert_agent_rows` where parent rows are
        // inserted with `agent_type = NULL`), and every non-NULL row
        // rolls up into the `subagent` bucket. Friendly labels are
        // assigned when we materialise the row — keeping labels
        // out of the SQL means the CASE only mentions the stable
        // keys, which is what the UI persists as the sort anchor.
        let mut stmt = connection
            .prepare(
                "SELECT CASE WHEN agent_type IS NULL
                             THEN 'main'
                             ELSE 'subagent'
                        END                        AS agent_key,
                        COUNT(DISTINCT turn_id)    AS turn_count,
                        COUNT(*)                   AS invocation_count,
                        COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_read_tokens), 0),
                        COALESCE(SUM(cache_write_tokens), 0),
                        COALESCE(SUM(cost_usd), 0.0),
                        MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
                 FROM usage_event_agents
                 WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
                 GROUP BY agent_key
                 ORDER BY SUM(cost_usd) DESC, COUNT(*) DESC",
            )
            .map_err(|e| format!("prepare by_agent_role: {e}"))?;

        let rows = stmt
            .query_map(params![from, to], |row| {
                let key: String = row.get(0)?;
                let label = if key == "main" {
                    "Main agent".to_string()
                } else {
                    "Subagents".to_string()
                };
                Ok(UsageAgentGroupRow {
                    key,
                    label,
                    turn_count: row.get::<_, i64>(1)? as u64,
                    invocation_count: row.get::<_, i64>(2)? as u64,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                    cache_write_tokens: row.get::<_, i64>(6)? as u64,
                    total_cost_usd: row.get(7)?,
                    cost_has_unknowns: row.get::<_, i64>(8)? != 0,
                })
            })
            .map_err(|e| format!("query by_agent_role: {e}"))?;
        let mut groups = Vec::new();
        for r in rows {
            groups.push(r.map_err(|e| format!("row by_agent_role: {e}"))?);
        }
        Ok(UsageAgentPayload {
            range,
            groups,
            generated_at: now.to_rfc3339(),
        })
    }

    /// Top sessions by total cost over the range. Uses `ORDER BY
    /// total_cost_usd DESC` with a secondary order on turn_count so
    /// free-tier sessions (cost = 0) still have a stable ranking by
    /// usage volume. `limit` is clamped to [1, 50].
    pub fn top_sessions(
        &self,
        range: UsageRange,
        limit: u32,
    ) -> Result<Vec<TopSessionRow>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Utc::now();
        let (from, to) = range.to_day_bounds(now);
        let capped = limit.clamp(1, 50) as i64;
        let mut stmt = connection
            .prepare(
                "SELECT session_id,
                        provider,
                        -- Prefer a non-empty model; otherwise NULL
                        MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END) AS model,
                        MAX(project_id) AS project_id,
                        COUNT(*) AS turn_count,
                        SUM(total_cost_usd) AS total_cost,
                        MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END) AS has_unknown,
                        MAX(occurred_at) AS last_at
                 FROM usage_events
                 WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
                 GROUP BY session_id, provider
                 ORDER BY total_cost DESC, turn_count DESC, last_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| format!("prepare top_sessions: {e}"))?;
        let rows = stmt
            .query_map(params![from, to, capped], |row| {
                let provider: String = row.get(1)?;
                let label = provider_label_from_tag(&provider);
                Ok(TopSessionRow {
                    session_id: row.get(0)?,
                    provider,
                    provider_label: label,
                    model: row.get(2)?,
                    project_id: row.get(3)?,
                    turn_count: row.get::<_, i64>(4)? as u64,
                    total_cost_usd: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    cost_has_unknowns: row.get::<_, i64>(6)? != 0,
                    last_activity_at: row.get(7)?,
                })
            })
            .map_err(|e| format!("query top_sessions: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("row top_sessions: {e}"))?);
        }
        Ok(out)
    }
}

/// Display-only label lookup for a provider tag stored in the usage
/// database. Unknown tags are surfaced verbatim rather than silently
/// coerced to `Codex` — if the rollup ever produces an unexpected
/// string we'd rather see it in the UI than hide the drift.
fn provider_label_from_tag(tag: &str) -> String {
    match ProviderKind::from_tag(tag) {
        Some(kind) => kind.label().to_string(),
        None => tag.to_string(),
    }
}

fn turn_status_to_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
    }
}

/// Insert per-agent slice rows for a freshly-recorded turn.
///
/// Allocates `event.total_cost_usd` across agents proportionally to
/// each agent's "billable weight" (input + output + cache_read +
/// cache_write tokens). The weight choice matches how the SDK itself
/// bills — every token type has a per-model rate that's > 0, so
/// using the sum is a close proxy for the cost share without needing
/// a per-model pricing table on the Rust side. When the total weight
/// is zero (no tokens at all) we skip the allocation and leave every
/// row at cost 0, which still satisfies `SUM(cost_usd) == 0`.
///
/// When the provider didn't supply an `agents` breakdown
/// (`event.agents.is_empty()`) we synthesize a single "main" row
/// carrying the turn's totals verbatim. That keeps the invariant
/// `SUM(usage_event_agents.cost_usd) == usage_events.total_cost_usd`
/// intact for every turn in the database, regardless of provider
/// support — the dashboard's By-Agent view can then treat missing
/// breakdowns as legitimate main-only turns.
fn insert_agent_rows(
    tx: &rusqlite::Transaction<'_>,
    event: &UsageEvent,
    day: &str,
    provider_str: &str,
) -> Result<(), String> {
    // Build the row set: synthesize a lone main row when the
    // provider gave us nothing, otherwise use the provider's slices
    // verbatim. The ordinal starts at 0 and increments in input order
    // so `ORDER BY agent_ordinal` is the natural dispatch order.
    let mut slices: Vec<UsageAgentSlice> = if event.agents.is_empty() {
        vec![UsageAgentSlice {
            agent_id: None,
            agent_type: None,
            model: event.model.clone(),
            input_tokens: event.input_tokens,
            output_tokens: event.output_tokens,
            cache_read_tokens: event.cache_read_tokens,
            cache_write_tokens: event.cache_write_tokens,
        }]
    } else {
        event.agents.clone()
    };

    // Proportional cost allocation. Compute weights and the cumulative
    // running remainder so the final agent absorbs any floating-point
    // drift — the invariant SUM(cost_usd) == event.total_cost_usd must
    // hold exactly for the rollup math to stay consistent with
    // `usage_events.total_cost_usd`.
    let weights: Vec<u64> = slices
        .iter()
        .map(|s| s.input_tokens + s.output_tokens + s.cache_read_tokens + s.cache_write_tokens)
        .collect();
    let total_weight: u64 = weights.iter().sum();
    let costs: Vec<f64> = if total_weight == 0 {
        // No tokens reported at all: the turn's cost is likely 0 too
        // (or unknown), and any nonzero total_cost_usd can't be
        // attributed. Give it all to the first (main) agent so the
        // sum still matches; this path is exceedingly rare in
        // practice.
        let mut c = vec![0.0_f64; slices.len()];
        if let Some(first) = c.first_mut() {
            *first = event.total_cost_usd;
        }
        c
    } else {
        // All but the last row get `floor(share * total_cost)`; the
        // last row absorbs the residual. This mirrors how we'd hand
        // out a fixed-point bill and guarantees the SUM invariant.
        let mut c = Vec::with_capacity(slices.len());
        let mut running = 0.0_f64;
        for (i, w) in weights.iter().enumerate() {
            if i + 1 == weights.len() {
                c.push(event.total_cost_usd - running);
            } else {
                let share = (*w as f64 / total_weight as f64) * event.total_cost_usd;
                running += share;
                c.push(share);
            }
        }
        c
    };

    let has_cost = if event.has_cost { 1_i64 } else { 0_i64 };
    for (ordinal, (slice, cost)) in slices.drain(..).zip(costs.into_iter()).enumerate() {
        tx.execute(
            "INSERT OR IGNORE INTO usage_event_agents (
                turn_id, agent_ordinal, agent_id, agent_type, model,
                input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens,
                cost_usd, has_cost,
                occurred_day_utc, provider, session_id
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7,
                ?8, ?9,
                ?10, ?11,
                ?12, ?13, ?14
             )",
            params![
                event.turn_id,
                ordinal as i64,
                slice.agent_id,
                slice.agent_type,
                slice.model,
                slice.input_tokens as i64,
                slice.output_tokens as i64,
                slice.cache_read_tokens as i64,
                slice.cache_write_tokens as i64,
                cost,
                has_cost,
                day,
                provider_str,
                event.session_id,
            ],
        )
        .map_err(|e| format!("insert usage_event_agents: {e}"))?;
    }
    Ok(())
}

/// Extract the UTC `YYYY-MM-DD` day from an RFC3339 timestamp.
/// Falls back to today's date on parse failure — unusual but
/// possible if a provider emits a malformed timestamp; we prefer to
/// still record the turn somewhere rather than drop it silently.
fn day_from_rfc3339(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| Utc::now().format("%Y-%m-%d").to_string())
}

/// Build the inclusive list of `YYYY-MM-DD` days between two
/// bounds. Used to zero-fill time series. Returns `[from]` on
/// malformed input so we never crash the query path.
fn day_axis(from: &str, to: &str) -> Vec<String> {
    let from_parsed = parse_day(from);
    let to_parsed = parse_day(to);
    let (Some(start), Some(end)) = (from_parsed, to_parsed) else {
        return vec![from.to_string()];
    };
    if end < start {
        return vec![to.to_string()];
    }
    let mut out = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        out.push(cursor.format("%Y-%m-%d").to_string());
        cursor += Duration::days(1);
    }
    out
}

fn parse_day(day: &str) -> Option<DateTime<Utc>> {
    let parsed = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let dt = parsed.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&dt))
}

fn earliest_event_day(connection: &Connection) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT MIN(occurred_day_utc) FROM usage_events",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| format!("query earliest day: {e}"))
}

fn read_totals(connection: &Connection, from: &str, to: &str) -> Result<UsageTotals, String> {
    let totals = connection
        .query_row(
            "SELECT
                COUNT(*) AS turn_count,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(total_cost_usd), 0.0),
                COALESCE(SUM(duration_ms), 0),
                COUNT(DISTINCT session_id),
                COUNT(DISTINCT
                    CASE WHEN model IS NOT NULL AND model != '' THEN model END),
                MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
             FROM usage_events
             WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2",
            params![from, to],
            |row| {
                let turn_count: i64 = row.get(0)?;
                let input_tokens: i64 = row.get(1)?;
                let output_tokens: i64 = row.get(2)?;
                let cache_read_tokens: i64 = row.get(3)?;
                let cache_write_tokens: i64 = row.get(4)?;
                let total_cost_usd: f64 = row.get(5)?;
                let total_duration_ms: i64 = row.get(6)?;
                let distinct_sessions: i64 = row.get(7)?;
                let distinct_models: i64 = row.get(8)?;
                let has_unknown: Option<i64> = row.get(9)?;
                Ok(UsageTotals {
                    turn_count: turn_count as u64,
                    input_tokens: input_tokens as u64,
                    output_tokens: output_tokens as u64,
                    cache_read_tokens: cache_read_tokens as u64,
                    cache_write_tokens: cache_write_tokens as u64,
                    total_cost_usd,
                    cost_has_unknowns: has_unknown.unwrap_or(0) != 0,
                    total_duration_ms: total_duration_ms as u64,
                    distinct_sessions: distinct_sessions as u64,
                    distinct_models: distinct_models as u64,
                })
            },
        )
        .map_err(|e| format!("query totals: {e}"))?;
    Ok(totals)
}

fn read_group(
    connection: &Connection,
    from: &str,
    to: &str,
    group_by: UsageGroupBy,
) -> Result<Vec<UsageGroupRow>, String> {
    // Whitelist the column so the GROUP BY / SELECT never embeds
    // caller-provided data.
    let (group_col, label_col) = match group_by {
        UsageGroupBy::ByProvider => ("provider", "provider"),
        UsageGroupBy::ByModel => ("COALESCE(model, '')", "COALESCE(model, '')"),
    };
    let sql = format!(
        "SELECT {group_col} AS group_key,
                {label_col} AS group_label,
                COUNT(*) AS turn_count,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(total_cost_usd), 0.0),
                COALESCE(SUM(duration_ms), 0),
                MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
         FROM usage_events
         WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
         GROUP BY {group_col}
         ORDER BY SUM(total_cost_usd) DESC, COUNT(*) DESC"
    );
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|e| format!("prepare group: {e}"))?;
    let rows = stmt
        .query_map(params![from, to], |row| {
            let key: String = row.get(0)?;
            let raw_label: String = row.get(1)?;
            let label = match group_by {
                UsageGroupBy::ByProvider => provider_label_from_tag(&raw_label),
                UsageGroupBy::ByModel => {
                    if raw_label.is_empty() {
                        "(unknown model)".to_string()
                    } else {
                        raw_label
                    }
                }
            };
            Ok(UsageGroupRow {
                key,
                label,
                turn_count: row.get::<_, i64>(2)? as u64,
                input_tokens: row.get::<_, i64>(3)? as u64,
                output_tokens: row.get::<_, i64>(4)? as u64,
                cache_read_tokens: row.get::<_, i64>(5)? as u64,
                cache_write_tokens: row.get::<_, i64>(6)? as u64,
                total_cost_usd: row.get(7)?,
                total_duration_ms: row.get::<_, i64>(8)? as u64,
                cost_has_unknowns: row.get::<_, i64>(9)? != 0,
            })
        })
        .map_err(|e| format!("query group: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("row group: {e}"))?);
    }
    Ok(out)
}

fn read_daily_points(
    connection: &Connection,
    from: &str,
    to: &str,
    axis: &[String],
) -> Result<Vec<UsageTimeseriesPoint>, String> {
    let mut stmt = connection
        .prepare(
            "SELECT occurred_day_utc,
                    COUNT(*) AS turn_count,
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(total_cost_usd), 0.0),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(DISTINCT session_id),
                    COUNT(DISTINCT
                        CASE WHEN model IS NOT NULL AND model != '' THEN model END),
                    MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
             FROM usage_events
             WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
             GROUP BY occurred_day_utc",
        )
        .map_err(|e| format!("prepare daily: {e}"))?;

    let mut totals_by_day: std::collections::HashMap<String, UsageTotals> =
        std::collections::HashMap::new();
    let rows = stmt
        .query_map(params![from, to], |row| {
            let day: String = row.get(0)?;
            let turn_count: i64 = row.get(1)?;
            Ok((
                day,
                UsageTotals {
                    turn_count: turn_count as u64,
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_tokens: row.get::<_, i64>(4)? as u64,
                    cache_write_tokens: row.get::<_, i64>(5)? as u64,
                    total_cost_usd: row.get(6)?,
                    total_duration_ms: row.get::<_, i64>(7)? as u64,
                    distinct_sessions: row.get::<_, i64>(8)? as u64,
                    distinct_models: row.get::<_, i64>(9)? as u64,
                    cost_has_unknowns: row.get::<_, i64>(10)? != 0,
                },
            ))
        })
        .map_err(|e| format!("query daily: {e}"))?;
    for r in rows {
        let (day, totals) = r.map_err(|e| format!("row daily: {e}"))?;
        totals_by_day.insert(day, totals);
    }

    let points = axis
        .iter()
        .map(|day| UsageTimeseriesPoint {
            bucket_start: day.clone(),
            totals: totals_by_day.remove(day).unwrap_or_else(|| empty_totals()),
        })
        .collect();
    Ok(points)
}

fn read_daily_series(
    connection: &Connection,
    from: &str,
    to: &str,
    split_by: UsageGroupBy,
    axis: &[String],
) -> Result<Vec<UsageSeries>, String> {
    let group_col = match split_by {
        UsageGroupBy::ByProvider => "provider",
        UsageGroupBy::ByModel => "COALESCE(model, '')",
    };
    let sql = format!(
        "SELECT {group_col} AS group_key,
                occurred_day_utc,
                COUNT(*) AS turn_count,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(total_cost_usd), 0.0),
                COALESCE(SUM(duration_ms), 0),
                COUNT(DISTINCT session_id),
                COUNT(DISTINCT
                    CASE WHEN model IS NOT NULL AND model != '' THEN model END),
                MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
         FROM usage_events
         WHERE occurred_day_utc >= ?1 AND occurred_day_utc <= ?2
         GROUP BY {group_col}, occurred_day_utc
         ORDER BY SUM(total_cost_usd) DESC"
    );
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|e| format!("prepare daily series: {e}"))?;

    type KeyDay = (String, String);
    let mut by_key_day: std::collections::HashMap<KeyDay, UsageTotals> =
        std::collections::HashMap::new();
    let mut key_order: Vec<String> = Vec::new();

    let rows = stmt
        .query_map(params![from, to], |row| {
            let key: String = row.get(0)?;
            let day: String = row.get(1)?;
            Ok((
                key,
                day,
                UsageTotals {
                    turn_count: row.get::<_, i64>(2)? as u64,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                    cache_write_tokens: row.get::<_, i64>(6)? as u64,
                    total_cost_usd: row.get(7)?,
                    total_duration_ms: row.get::<_, i64>(8)? as u64,
                    distinct_sessions: row.get::<_, i64>(9)? as u64,
                    distinct_models: row.get::<_, i64>(10)? as u64,
                    cost_has_unknowns: row.get::<_, i64>(11)? != 0,
                },
            ))
        })
        .map_err(|e| format!("query daily series: {e}"))?;
    for r in rows {
        let (key, day, totals) = r.map_err(|e| format!("row daily series: {e}"))?;
        if !key_order.contains(&key) {
            key_order.push(key.clone());
        }
        by_key_day.insert((key, day), totals);
    }

    let mut out = Vec::new();
    for key in key_order {
        let label = match split_by {
            UsageGroupBy::ByProvider => provider_label_from_tag(&key),
            UsageGroupBy::ByModel => {
                if key.is_empty() {
                    "(unknown model)".to_string()
                } else {
                    key.clone()
                }
            }
        };
        let points = axis
            .iter()
            .map(|day| UsageTimeseriesPoint {
                bucket_start: day.clone(),
                totals: by_key_day
                    .remove(&(key.clone(), day.clone()))
                    .unwrap_or_else(|| empty_totals()),
            })
            .collect();
        out.push(UsageSeries { key, label, points });
    }
    Ok(out)
}

fn empty_totals() -> UsageTotals {
    UsageTotals {
        turn_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        total_cost_usd: 0.0,
        cost_has_unknowns: false,
        total_duration_ms: 0,
        distinct_sessions: 0,
        distinct_models: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(
        turn_id: &str,
        session_id: &str,
        provider: ProviderKind,
        model: Option<&str>,
        occurred_at: &str,
        input: u64,
        output: u64,
        cost: Option<f64>,
    ) -> UsageEvent {
        UsageEvent {
            turn_id: turn_id.to_string(),
            session_id: session_id.to_string(),
            provider,
            model: model.map(|m| m.to_string()),
            project_id: None,
            status: TurnStatus::Completed,
            occurred_at: occurred_at.to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_cost_usd: cost.unwrap_or(0.0),
            has_cost: cost.is_some(),
            duration_ms: 1_000,
            agents: Vec::new(),
        }
    }

    /// Assert that for every day/provider/model the rollup table
    /// agrees with the SUM over usage_events. Catches off-by-one or
    /// skipped-update bugs in the upsert path.
    fn assert_rollups_match_events(store: &UsageStore) {
        let connection = store.connection.lock().unwrap();
        let mut stmt = connection
            .prepare(
                "SELECT occurred_day_utc,
                        provider,
                        COALESCE(model, '') AS model_key,
                        COUNT(*),
                        SUM(input_tokens),
                        SUM(output_tokens),
                        SUM(total_cost_usd),
                        MAX(CASE WHEN has_cost = 0 THEN 1 ELSE 0 END)
                 FROM usage_events
                 GROUP BY occurred_day_utc, provider, model_key
                 ORDER BY occurred_day_utc, provider, model_key",
            )
            .unwrap();
        let mut events: Vec<(String, String, String, i64, i64, i64, f64, i64)> = Vec::new();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })
            .unwrap();
        for r in rows {
            events.push(r.unwrap());
        }

        for (day, provider, model, count, input, output, cost, has_unk) in events {
            let (rc_count, rc_input, rc_output, rc_cost, rc_has_unk): (i64, i64, i64, f64, i64) =
                connection
                    .query_row(
                        "SELECT turn_count, input_tokens, output_tokens, total_cost_usd, has_unknown_cost
                         FROM usage_daily_rollups
                         WHERE day_utc = ?1 AND provider = ?2 AND model = ?3",
                        params![day, provider, model],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .expect("rollup row missing for events key");
            assert_eq!(
                rc_count, count,
                "turn_count mismatch for ({day}, {provider}, {model})"
            );
            assert_eq!(rc_input, input, "input_tokens mismatch");
            assert_eq!(rc_output, output, "output_tokens mismatch");
            assert!(
                (rc_cost - cost).abs() < 1e-9,
                "cost mismatch: rollup={rc_cost} events={cost}"
            );
            assert_eq!(rc_has_unk, has_unk, "has_unknown_cost mismatch");
        }
    }

    #[test]
    fn record_and_rollup_match_events() {
        let store = UsageStore::in_memory().unwrap();
        let events = [
            sample_event(
                "t1",
                "s1",
                ProviderKind::Claude,
                Some("claude-sonnet"),
                "2026-04-15T12:00:00Z",
                100,
                200,
                Some(0.01),
            ),
            sample_event(
                "t2",
                "s1",
                ProviderKind::Claude,
                Some("claude-sonnet"),
                "2026-04-15T13:00:00Z",
                150,
                250,
                Some(0.02),
            ),
            sample_event(
                "t3",
                "s2",
                ProviderKind::Codex,
                Some("gpt-5"),
                "2026-04-15T14:00:00Z",
                80,
                120,
                None,
            ),
            sample_event(
                "t4",
                "s3",
                ProviderKind::Claude,
                Some("claude-opus"),
                "2026-04-16T10:00:00Z",
                200,
                300,
                Some(0.10),
            ),
        ];
        for e in events.iter() {
            store.record_turn(e).unwrap();
        }
        assert_rollups_match_events(&store);
    }

    #[test]
    fn duplicate_turn_ids_are_no_ops() {
        let store = UsageStore::in_memory().unwrap();
        // Use a timestamp 1 day ago (always inside the Last7Days
        // window) so the test isn't date-sensitive — the previous
        // hardcoded `2026-04-15` silently aged out of the window the
        // moment the calendar crossed +7d, making the test a flaky
        // time-bomb.
        let recent = (Utc::now() - Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let event = sample_event(
            "t1",
            "s1",
            ProviderKind::Claude,
            Some("claude-sonnet"),
            &recent,
            100,
            200,
            Some(0.01),
        );
        store.record_turn(&event).unwrap();
        // Replay the same turn_id. Both event and rollup counts
        // must stay at 1, totals unchanged.
        store.record_turn(&event).unwrap();
        store.record_turn(&event).unwrap();

        let summary = store
            .summary(UsageRange::Last7Days, UsageGroupBy::ByProvider)
            .unwrap();
        assert_eq!(summary.totals.turn_count, 1);
        assert_eq!(summary.totals.input_tokens, 100);
        assert_eq!(summary.totals.output_tokens, 200);
        assert!((summary.totals.total_cost_usd - 0.01).abs() < 1e-9);
        assert_rollups_match_events(&store);
    }

    #[test]
    fn range_filters_events_outside_window() {
        let store = UsageStore::in_memory().unwrap();
        let today = Utc::now();
        // 3 days ago — inside 7d window.
        let inside = (today - Duration::days(3))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        // 40 days ago — outside 7d, inside 90d.
        let outside = (today - Duration::days(40))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        store
            .record_turn(&sample_event(
                "inside",
                "s1",
                ProviderKind::Claude,
                Some("m"),
                &inside,
                10,
                20,
                Some(0.01),
            ))
            .unwrap();
        store
            .record_turn(&sample_event(
                "outside",
                "s2",
                ProviderKind::Claude,
                Some("m"),
                &outside,
                10,
                20,
                Some(0.01),
            ))
            .unwrap();

        let seven = store
            .summary(UsageRange::Last7Days, UsageGroupBy::ByProvider)
            .unwrap();
        assert_eq!(seven.totals.turn_count, 1);

        let ninety = store
            .summary(UsageRange::Last90Days, UsageGroupBy::ByProvider)
            .unwrap();
        assert_eq!(ninety.totals.turn_count, 2);

        let all = store
            .summary(UsageRange::AllTime, UsageGroupBy::ByProvider)
            .unwrap();
        assert_eq!(all.totals.turn_count, 2);
    }

    #[test]
    fn timeseries_is_zero_filled_over_range() {
        let store = UsageStore::in_memory().unwrap();
        // Single event three days ago — the other six days of the
        // 7-day window must still appear with zero totals.
        let three_days_ago = (Utc::now() - Duration::days(3))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        store
            .record_turn(&sample_event(
                "t1",
                "s1",
                ProviderKind::Claude,
                Some("m"),
                &three_days_ago,
                10,
                20,
                Some(0.01),
            ))
            .unwrap();
        let ts = store
            .timeseries(UsageRange::Last7Days, UsageBucket::Daily, None)
            .unwrap();
        assert_eq!(ts.points.len(), 7);
        let non_zero = ts.points.iter().filter(|p| p.totals.turn_count > 0).count();
        assert_eq!(non_zero, 1);
        let total_turns: u64 = ts.points.iter().map(|p| p.totals.turn_count).sum();
        assert_eq!(total_turns, 1);
    }

    #[test]
    fn cost_has_unknowns_propagates() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        store
            .record_turn(&sample_event(
                "known",
                "s1",
                ProviderKind::Claude,
                Some("m"),
                &now_ish,
                10,
                20,
                Some(0.01),
            ))
            .unwrap();
        store
            .record_turn(&sample_event(
                "unknown",
                "s1",
                ProviderKind::Claude,
                Some("m"),
                &now_ish,
                10,
                20,
                None,
            ))
            .unwrap();
        let summary = store
            .summary(UsageRange::Last7Days, UsageGroupBy::ByProvider)
            .unwrap();
        assert!(summary.totals.cost_has_unknowns);
        assert_eq!(summary.by_provider.len(), 1);
        assert!(summary.by_provider[0].cost_has_unknowns);
    }

    #[test]
    fn group_by_model_aggregates_correctly() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        store
            .record_turn(&sample_event(
                "t1",
                "s1",
                ProviderKind::Claude,
                Some("sonnet"),
                &now_ish,
                100,
                200,
                Some(0.01),
            ))
            .unwrap();
        store
            .record_turn(&sample_event(
                "t2",
                "s2",
                ProviderKind::Claude,
                Some("opus"),
                &now_ish,
                100,
                200,
                Some(0.10),
            ))
            .unwrap();
        store
            .record_turn(&sample_event(
                "t3",
                "s3",
                ProviderKind::Claude,
                Some("sonnet"),
                &now_ish,
                50,
                100,
                Some(0.005),
            ))
            .unwrap();
        let summary = store
            .summary(UsageRange::Last7Days, UsageGroupBy::ByModel)
            .unwrap();
        assert_eq!(summary.groups.len(), 2);
        // Ordered by total_cost_usd DESC — opus first.
        assert_eq!(summary.groups[0].key, "opus");
        assert_eq!(summary.groups[0].turn_count, 1);
        assert_eq!(summary.groups[1].key, "sonnet");
        assert_eq!(summary.groups[1].turn_count, 2);
    }

    #[test]
    fn top_sessions_ranks_by_cost() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        for (tid, sid, cost) in [
            ("t1", "cheap", 0.001),
            ("t2", "expensive", 0.50),
            ("t3", "expensive", 0.25),
            ("t4", "mid", 0.10),
        ] {
            store
                .record_turn(&sample_event(
                    tid,
                    sid,
                    ProviderKind::Claude,
                    Some("m"),
                    &now_ish,
                    10,
                    20,
                    Some(cost),
                ))
                .unwrap();
        }
        let top = store.top_sessions(UsageRange::Last7Days, 10).unwrap();
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].session_id, "expensive");
        assert!((top[0].total_cost_usd - 0.75).abs() < 1e-9);
        assert_eq!(top[0].turn_count, 2);
        assert_eq!(top[1].session_id, "mid");
        assert_eq!(top[2].session_id, "cheap");
    }

    #[test]
    fn limit_is_clamped() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        store
            .record_turn(&sample_event(
                "t1",
                "s1",
                ProviderKind::Claude,
                Some("m"),
                &now_ish,
                10,
                20,
                Some(0.01),
            ))
            .unwrap();
        // limit=0 clamps to 1.
        let top = store.top_sessions(UsageRange::Last7Days, 0).unwrap();
        assert_eq!(top.len(), 1);
        // limit=9999 clamps to 50 (we only have 1 row so we see 1).
        let top = store.top_sessions(UsageRange::Last7Days, 9_999).unwrap();
        assert_eq!(top.len(), 1);
    }

    /// Turn with no per-agent breakdown supplied: the store must
    /// synthesize a single main-agent row so the invariant
    /// `SUM(usage_event_agents.cost_usd) == usage_events.total_cost_usd`
    /// still holds and the By-Agent view treats legacy / non-SDK
    /// providers as all-main.
    #[test]
    fn by_agent_synthesizes_main_when_breakdown_missing() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        store
            .record_turn(&sample_event(
                "t1",
                "s1",
                ProviderKind::Claude,
                Some("sonnet"),
                &now_ish,
                100,
                200,
                Some(0.05),
            ))
            .unwrap();
        let payload = store.summary_by_agent(UsageRange::Last7Days).unwrap();
        assert_eq!(payload.groups.len(), 1);
        let row = &payload.groups[0];
        assert_eq!(row.key, "main");
        assert_eq!(row.label, "Main agent");
        assert_eq!(row.turn_count, 1);
        assert_eq!(row.invocation_count, 1);
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.output_tokens, 200);
        assert!((row.total_cost_usd - 0.05).abs() < 1e-9);
    }

    /// Turn with a full provider-supplied breakdown: one main bucket
    /// plus two Explore subagents. Verifies that (a) cost is
    /// allocated proportionally to the billable-weight sum and (b)
    /// summing per-agent cost over the turn reconciles exactly with
    /// the turn-level total.
    #[test]
    fn by_agent_allocates_cost_proportionally_to_weight() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        // Main agent: 100 + 200 + 0 + 0 = 300 weight
        // Explore #1: 50 + 100 + 50 + 0 = 200 weight
        // Explore #2: 50 + 100 + 0 + 50 = 200 weight
        // Total weight = 700; main should get 300/700 * $0.70 = $0.30
        let event = UsageEvent {
            turn_id: "t1".into(),
            session_id: "s1".into(),
            provider: ProviderKind::Claude,
            model: Some("sonnet".into()),
            project_id: None,
            status: TurnStatus::Completed,
            occurred_at: now_ish.clone(),
            input_tokens: 200,
            output_tokens: 400,
            cache_read_tokens: 50,
            cache_write_tokens: 50,
            total_cost_usd: 0.70,
            has_cost: true,
            duration_ms: 1_000,
            agents: vec![
                UsageAgentSlice {
                    agent_id: None,
                    agent_type: None,
                    model: Some("sonnet".into()),
                    input_tokens: 100,
                    output_tokens: 200,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                UsageAgentSlice {
                    agent_id: Some("call_abc".into()),
                    agent_type: Some("Explore".into()),
                    model: Some("sonnet".into()),
                    input_tokens: 50,
                    output_tokens: 100,
                    cache_read_tokens: 50,
                    cache_write_tokens: 0,
                },
                UsageAgentSlice {
                    agent_id: Some("call_def".into()),
                    agent_type: Some("Explore".into()),
                    model: Some("sonnet".into()),
                    input_tokens: 50,
                    output_tokens: 100,
                    cache_read_tokens: 0,
                    cache_write_tokens: 50,
                },
            ],
        };
        store.record_turn(&event).unwrap();

        let payload = store.summary_by_agent(UsageRange::Last7Days).unwrap();
        assert_eq!(payload.groups.len(), 2);
        // Explore shows up first because its total cost ($0.40)
        // exceeds main ($0.30).
        let explore = payload.groups.iter().find(|r| r.key == "Explore").unwrap();
        let main = payload.groups.iter().find(|r| r.key == "main").unwrap();
        assert_eq!(explore.invocation_count, 2);
        assert_eq!(explore.turn_count, 1);
        assert!((explore.total_cost_usd - 0.40).abs() < 1e-9);
        assert!((main.total_cost_usd - 0.30).abs() < 1e-9);
        // SUM invariant holds exactly (last-bucket absorbs rounding).
        let sum: f64 = payload.groups.iter().map(|r| r.total_cost_usd).sum();
        assert!((sum - 0.70).abs() < 1e-9);

        // Same turn, rolled up by role: exactly two rows, and the
        // subagent bucket folds both Explore invocations together
        // ($0.20 each → $0.40 total) while main's $0.30 is preserved.
        // Counts: main = 1 invocation / 1 turn; subagent = 2 invocations / 1 turn.
        let roles = store.summary_by_agent_role(UsageRange::Last7Days).unwrap();
        assert_eq!(roles.groups.len(), 2);
        let role_sub = roles.groups.iter().find(|r| r.key == "subagent").unwrap();
        let role_main = roles.groups.iter().find(|r| r.key == "main").unwrap();
        assert_eq!(role_sub.label, "Subagents");
        assert_eq!(role_main.label, "Main agent");
        assert_eq!(role_sub.invocation_count, 2);
        assert_eq!(role_sub.turn_count, 1);
        assert_eq!(role_main.invocation_count, 1);
        assert_eq!(role_main.turn_count, 1);
        assert!((role_sub.total_cost_usd - 0.40).abs() < 1e-9);
        assert!((role_main.total_cost_usd - 0.30).abs() < 1e-9);
        let role_sum: f64 = roles.groups.iter().map(|r| r.total_cost_usd).sum();
        assert!((role_sum - 0.70).abs() < 1e-9);
    }

    /// Replay of the same turn_id must not double-insert agent rows.
    /// The `INSERT OR IGNORE` on the composite PK guarantees idempotency.
    #[test]
    fn by_agent_idempotent_on_turn_replay() {
        let store = UsageStore::in_memory().unwrap();
        let now_ish = Utc::now().format("%Y-%m-%dT12:00:00Z").to_string();
        let event = UsageEvent {
            turn_id: "t1".into(),
            session_id: "s1".into(),
            provider: ProviderKind::Claude,
            model: Some("sonnet".into()),
            project_id: None,
            status: TurnStatus::Completed,
            occurred_at: now_ish,
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_cost_usd: 0.10,
            has_cost: true,
            duration_ms: 1_000,
            agents: vec![UsageAgentSlice {
                agent_id: None,
                agent_type: None,
                model: Some("sonnet".into()),
                input_tokens: 100,
                output_tokens: 200,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }],
        };
        store.record_turn(&event).unwrap();
        store.record_turn(&event).unwrap();
        store.record_turn(&event).unwrap();
        let payload = store.summary_by_agent(UsageRange::Last7Days).unwrap();
        assert_eq!(payload.groups.len(), 1);
        assert_eq!(payload.groups[0].invocation_count, 1);
        assert_eq!(payload.groups[0].turn_count, 1);
        assert!((payload.groups[0].total_cost_usd - 0.10).abs() < 1e-9);
    }
}
