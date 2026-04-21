//! Thin `reqwest` wrapper around the subset of opencode's REST API
//! that flowstate actually consumes.
//!
//! Opencode's server speaks OpenAPI 3.1 and the full surface is
//! sizeable (session CRUD, tool catalog, file I/O helpers, TUI
//! control, experimental endpoints). We only wrap the endpoints we
//! exercise from the adapter — health, session lifecycle, prompt
//! dispatch, abort, permission answers, and the model catalog. Each
//! wrapper uses `serde_json::Value` for response payloads unless
//! there's a field we actually read; that keeps the adapter tolerant
//! of opencode schema evolution without maintaining a hand-mirrored
//! copy of the spec.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tracing::debug;
use zenui_provider_api::{PermissionDecision, PermissionMode, ProviderModel, ReasoningEffort};

/// Opencode's permission-reply vocabulary. Mapping from flowstate's
/// [`PermissionDecision`] lives on the enum so both the SSE event
/// handler and any direct caller use the same translation table.
#[derive(Debug, Clone, Copy)]
pub enum PermissionReply {
    /// Approve this one invocation. Opencode won't remember.
    Once,
    /// Approve and remember for the rest of the opencode session.
    /// Maps to flowstate's `AllowAlways`.
    Always,
    /// Deny this invocation. Opencode has no symmetric "deny
    /// always" — we collapse `DenyAlways` onto `Reject` because the
    /// runtime caches denies in its own session-scoped policy map,
    /// so a future attempt short-circuits before the request ever
    /// reaches opencode again.
    Reject,
}

impl PermissionReply {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }

    /// Translate flowstate's permission vocabulary to opencode's.
    pub fn from_decision(decision: PermissionDecision) -> Self {
        match decision {
            PermissionDecision::Allow => Self::Once,
            PermissionDecision::AllowAlways => Self::Always,
            PermissionDecision::Deny | PermissionDecision::DenyAlways => Self::Reject,
        }
    }
}

/// Default HTTP timeout for control-plane requests (create session,
/// abort, permission answer, health). Turn-level work streams through
/// SSE so this timeout never gates model wait times — if a control
/// request takes more than a couple of seconds something is wrong.
const CONTROL_TIMEOUT_SECS: u64 = 15;

pub struct OpenCodeClient {
    base_url: String,
    /// Opencode's default server username when
    /// `OPENCODE_SERVER_USERNAME` is unset. Documented as `"opencode"`.
    /// Pair with the randomly generated password held by
    /// [`crate::server::OpenCodeServer`].
    username: String,
    password: String,
    http: Client,
}

impl OpenCodeClient {
    pub fn new(base_url: String, password: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(CONTROL_TIMEOUT_SECS))
            // SSE requests override this below; keeping a default here
            // avoids hanging forever on a control-plane stall.
            .build()
            .expect("reqwest client build should not fail on valid defaults");
        Self {
            base_url,
            username: "opencode".to_string(),
            password,
            http,
        }
    }

    /// Base URL (e.g. `http://127.0.0.1:49192`). Exposed for the SSE
    /// reader, which opens its own long-lived connection against the
    /// same host but wants to build a client without the control-plane
    /// timeout.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Basic-auth credentials for the SSE reader to reuse. The SSE
    /// client is separate from `self.http` because it needs an
    /// unbounded timeout — there's no point reusing the tuned
    /// control-plane client.
    pub fn credentials(&self) -> (&str, &str) {
        (&self.username, &self.password)
    }

    /// Quick liveness probe. Returns `Ok(())` when the server answers
    /// a 2xx on its health endpoint. Errors carry the HTTP status
    /// (when reachable) or the underlying transport error.
    pub async fn health(&self) -> Result<(), String> {
        let response = self
            .request_get("/app")
            .await
            .map_err(|e| format!("opencode health probe failed to dispatch: {e}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "opencode health probe returned {}",
                response.status()
            ))
        }
    }

    /// Create a new opencode session rooted at `directory`. Returns
    /// the opaque session id the opencode server assigned — we keep
    /// that as our `ProviderSessionState::native_thread_id`.
    ///
    /// `permission_mode` controls which tool categories the agent
    /// can invoke without prompting the user; see
    /// [`permission_rules_for`] for the exact translation.
    pub async fn create_session(
        &self,
        directory: &str,
        model: Option<&str>,
        permission_mode: PermissionMode,
    ) -> Result<String, String> {
        let mut body = json!({
            "directory": directory,
            "permission": permission_rules_for(permission_mode),
        });
        if let Some(model) = model {
            if let Some(obj) = parse_model_slug(model) {
                body["model"] = obj;
            }
        }

        let response = self
            .request_post("/session", &body)
            .await
            .map_err(|e| format!("opencode session create failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "opencode session create returned {status}: {body}"
            ));
        }

        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("opencode session create: decoding JSON failed: {e}"))?;

        // Accept either `{ id }` on the root or a nested `{ info: { id } }`
        // shape — opencode's OpenAPI has shifted between the two across
        // versions and the cost of matching both up front is low.
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            return Ok(id.to_string());
        }
        if let Some(id) = payload
            .get("info")
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
        {
            return Ok(id.to_string());
        }

        Err(format!(
            "opencode session create: could not find session id in response body: {payload}"
        ))
    }

    /// Enqueue a user prompt on an existing session. Returns as soon
    /// as opencode has accepted the request; all streaming (text
    /// tokens, tool calls, completion) arrives via the SSE channel.
    ///
    /// `reasoning_effort` translates to opencode's per-model
    /// `variant` field — models with reasoning variants expose them
    /// under keys like `"low" | "medium" | "high" | "xhigh" | "max"`
    /// in `/config/providers`, and opencode applies the matching
    /// thinking budget when the caller names one on the prompt body.
    /// We omit the field when the effort is `None` so models without
    /// variants don't get a rejected request.
    ///
    /// `permission_mode` selects an opencode **agent** — the primary
    /// plan-vs-act signal. `PermissionMode::Plan` sends
    /// `agent: "plan"` so opencode flips into its first-class plan
    /// agent (`"Plan mode. Disallows all edit tools."`); every other
    /// mode omits the field and falls back to opencode's default
    /// `build` agent. Any session-level `permission` ruleset (set on
    /// `POST /session`) still applies.
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        model: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        permission_mode: PermissionMode,
    ) -> Result<(), String> {
        let mut body = json!({
            "parts": [{ "type": "text", "text": text }],
        });
        if let Some(model) = model {
            if let Some(obj) = parse_model_slug(model) {
                body["model"] = obj;
            }
        }
        if let Some(variant) = reasoning_effort.and_then(effort_to_variant) {
            body["variant"] = Value::String(variant.to_string());
        }
        if let Some(agent) = agent_for(permission_mode) {
            body["agent"] = Value::String(agent.to_string());
        }

        let path = format!("/session/{session_id}/prompt_async");
        let response = self
            .request_post(&path, &body)
            .await
            .map_err(|e| format!("opencode prompt dispatch failed: {e}"))?;

        let status = response.status();
        // Per the docs `/prompt_async` returns 204 No Content. Some
        // older builds return 200 with an empty body — both are fine.
        if status == StatusCode::NO_CONTENT || status.is_success() {
            debug!(%session_id, "opencode prompt accepted ({status})");
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(format!(
                "opencode prompt returned {status}: {body}"
            ))
        }
    }

    /// Abort an in-flight turn. Idempotent — calling `abort` on a
    /// session that isn't actively running succeeds as a no-op from
    /// the caller's perspective.
    pub async fn abort_session(&self, session_id: &str) -> Result<(), String> {
        let path = format!("/session/{session_id}/abort");
        let response = self
            .request_post(&path, &json!({}))
            .await
            .map_err(|e| format!("opencode abort dispatch failed: {e}"))?;
        if response.status().is_success() || response.status() == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            Err(format!(
                "opencode abort returned {}",
                response.status()
            ))
        }
    }

    /// Answer a pending `question.asked` event (the opencode "ask-user"
    /// tool flow — agent-driven clarifying questions, distinct from
    /// the permission system).
    ///
    /// Endpoint verified via live probe (see `/tmp/opencode-probe/probe3.mjs`):
    /// `POST /question/{id}/reply` is the only route that returns a
    /// non-HTML success response. Body shape is
    /// `{ requestID, answers: [[label, ...], ...] }` — one inner array
    /// per question the event carried (a single `question.asked`
    /// event may bundle multiple questions), each holding the chosen
    /// option labels.
    pub async fn respond_to_question(
        &self,
        request_id: &str,
        answers: Vec<Vec<String>>,
    ) -> Result<(), String> {
        let path = format!("/question/{request_id}/reply");
        let body = json!({ "requestID": request_id, "answers": answers });
        let response = self
            .request_post(&path, &body)
            .await
            .map_err(|e| format!("opencode question.reply failed: {e}"))?;
        if response.status().is_success() || response.status() == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!(
                "opencode question.reply returned {status}: {body}"
            ))
        }
    }

    /// Answer a pending permission prompt.
    ///
    /// Opencode accepts `reply` ∈ `{"once", "always", "reject"}`:
    /// - `once` — approve this single invocation
    /// - `always` — approve now and remember for the session
    /// - `reject` — deny this invocation
    ///
    /// There is no separate "deny forever" reply — a `reject` is
    /// always scoped to the current request. Our [`crate::events`]
    /// layer collapses flowstate's `DenyAlways` down to `reject`
    /// because the runtime also caches the deny in its own policy
    /// map (so the next attempt short-circuits before ever hitting
    /// opencode).
    pub async fn respond_to_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        reply: PermissionReply,
    ) -> Result<(), String> {
        let path = format!("/session/{session_id}/permissions/{permission_id}");
        let body = json!({ "reply": reply.as_wire_str() });
        let response = self
            .request_post(&path, &body)
            .await
            .map_err(|e| format!("opencode permission response failed: {e}"))?;
        if response.status().is_success() || response.status() == StatusCode::NO_CONTENT {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!(
                "opencode permission response returned {status}: {body}"
            ))
        }
    }

    /// Fetch the model catalog opencode is configured with (one entry
    /// per provider/model combination). Populates the session model
    /// dropdown.
    ///
    /// Opencode's providers endpoint nests models under each provider;
    /// we flatten into a single list using `"{providerID}/{modelID}"`
    /// as the stable id so users see which upstream is being called.
    pub async fn list_models(&self) -> Result<Vec<ProviderModel>, String> {
        let response = self
            .request_get("/config/providers")
            .await
            .map_err(|e| format!("opencode list_models dispatch failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "opencode list_models returned {}",
                response.status()
            ));
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("opencode list_models: decode failed: {e}"))?;

        let providers = payload
            .get("providers")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| payload.as_array().cloned())
            .unwrap_or_default();

        let mut models = Vec::new();
        for provider in providers {
            let provider_id = provider
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let provider_name = provider
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(provider_id);
            let Some(model_map) = provider.get("models") else {
                continue;
            };
            // Opencode has returned both `[{ id, name }, …]` and
            // `{ "model-id": { name, ... } }` over time. Handle both.
            if let Some(arr) = model_map.as_array() {
                for model in arr {
                    let id = model
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if id.is_empty() {
                        continue;
                    }
                    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);
                    let variants = variant_keys(model);
                    models.push(ProviderModel {
                        value: format!("{provider_id}/{id}"),
                        label: format!("{name} \u{00b7} {provider_name}"),
                        is_free: is_free_model(provider_id, model),
                        supports_effort: !variants.is_empty(),
                        supported_effort_levels: variants,
                        ..Default::default()
                    });
                }
            } else if let Some(obj) = model_map.as_object() {
                for (id, model) in obj {
                    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);
                    let variants = variant_keys(model);
                    models.push(ProviderModel {
                        value: format!("{provider_id}/{id}"),
                        label: format!("{name} \u{00b7} {provider_name}"),
                        is_free: is_free_model(provider_id, model),
                        supports_effort: !variants.is_empty(),
                        supported_effort_levels: variants,
                        ..Default::default()
                    });
                }
            }
        }
        Ok(models)
    }

    // ── internal helpers ────────────────────────────────────────────

    async fn request_get(&self, path: &str) -> reqwest::Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        self.http
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
    }

    async fn request_post(&self, path: &str, body: &Value) -> reqwest::Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, path);
        self.http
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .json(body)
            .send()
            .await
    }
}

/// Translate flowstate's `PermissionMode` into an opencode permission
/// ruleset. Opencode accepts this on session-create under `permission`
/// and enforces it for every subsequent turn on that session.
///
/// Rule shape (verified in the wild, see `/tmp/opencode-probe/probe4.mjs`):
///   `[{ permission, pattern, action: "allow" | "ask" | "deny" }]`
///
/// The permission categories opencode recognises at the time of
/// writing:
///   - `bash`, `edit`, `webfetch`, `websearch`, `codesearch`,
///     `external_directory`, `doom_loop`, `question`
///   - `*` matches any category
///
/// Mapping rationale:
///   - `Bypass`         → allow everything. Matches Claude's
///                        `bypassPermissions` behaviour.
///   - `AcceptEdits`    → allow file writes, ask for anything
///                        side-effectful (bash, webfetch, …). Mirrors
///                        the Claude CLI's default convenience mode.
///   - `Plan`           → a permissive "allow reads" ruleset. The
///                        plan-vs-act contract is owned by
///                        `agent: "plan"` on the prompt body (see
///                        [`agent_for`]); opencode's plan agent
///                        already denies edit tools internally, so
///                        we don't double-enforce here — duplicate
///                        denials would just race the agent's own
///                        shape and produce confusing error surfaces.
///   - `Default` / `Auto` → ask on every non-trivial category.
///                          Opencode doesn't have a classifier-based
///                          auto mode yet, so Auto degrades to
///                          Default on the wire.
///   - `question` is always allowed — it's the agent's built-in
///                     ask-user flow and blocking it would silently
///                     hang tool-using turns.
pub fn permission_rules_for(mode: PermissionMode) -> Value {
    match mode {
        PermissionMode::Bypass => {
            json!([{ "permission": "*", "pattern": "*", "action": "allow" }])
        }
        PermissionMode::AcceptEdits => json!([
            { "permission": "edit",       "pattern": "*", "action": "allow" },
            { "permission": "question",   "pattern": "*", "action": "allow" },
            { "permission": "bash",       "pattern": "*", "action": "ask"   },
            { "permission": "webfetch",   "pattern": "*", "action": "ask"   },
            { "permission": "websearch",  "pattern": "*", "action": "ask"   },
            { "permission": "codesearch", "pattern": "*", "action": "allow" },
            { "permission": "external_directory", "pattern": "*", "action": "ask" },
            { "permission": "doom_loop",  "pattern": "*", "action": "ask"   },
        ]),
        // Plan mode: let opencode's built-in `plan` agent own the
        // contract. We keep the ruleset minimal + readable — the
        // agent internally denies every edit tool, so stacking our
        // own denies here would be noise.
        PermissionMode::Plan => json!([
            { "permission": "question",   "pattern": "*", "action": "allow" },
            { "permission": "codesearch", "pattern": "*", "action": "allow" },
            { "permission": "websearch",  "pattern": "*", "action": "allow" },
        ]),
        PermissionMode::Default | PermissionMode::Auto => json!([
            { "permission": "question",   "pattern": "*", "action": "allow" },
            { "permission": "codesearch", "pattern": "*", "action": "allow" },
            { "permission": "bash",       "pattern": "*", "action": "ask"   },
            { "permission": "edit",       "pattern": "*", "action": "ask"   },
            { "permission": "webfetch",   "pattern": "*", "action": "ask"   },
            { "permission": "websearch",  "pattern": "*", "action": "ask"   },
            { "permission": "external_directory", "pattern": "*", "action": "ask" },
            { "permission": "doom_loop",  "pattern": "*", "action": "ask"   },
        ]),
    }
}

/// Extract the variant names a model exposes under its `variants`
/// object.
///
/// Opencode's catalogue puts per-model reasoning variants under
/// `variants: { low: {...}, medium: {...}, high: {...}, ... }`;
/// the keys are the strings our adapter sends on
/// `POST /session/:id/prompt_async` under `variant`.
///
/// Only keys matching flowstate's canonical effort levels are
/// surfaced (`low`, `medium`, `high`, `xhigh`, `max`) — opencode
/// occasionally ships variant keys that don't map cleanly onto
/// flowstate's `ReasoningEffort` enum (e.g. `"codex-low"`,
/// provider-specific flavours); dropping them here keeps the
/// effort selector's options aligned with values the adapter can
/// actually round-trip.
///
/// Returns an empty `Vec` when the model has no variants object
/// (catalogue entries without reasoning support) or none of its
/// keys are recognised; that's what `ProviderModel.supports_effort`
/// checks to decide whether to render the selector at all.
fn variant_keys(model: &Value) -> Vec<String> {
    const KNOWN: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    let Some(variants) = model.get("variants").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<String> = variants
        .keys()
        .filter(|k| KNOWN.contains(&k.as_str()))
        .map(|k| k.to_string())
        .collect();
    // Stable order for tests / UI: follow the enum's natural
    // ascending intensity rather than hash-map iteration order.
    out.sort_by_key(|k| KNOWN.iter().position(|x| *x == k).unwrap_or(usize::MAX));
    out
}

/// Pick an opencode agent for the caller's permission mode.
///
/// Opencode's agent registry (live-listed via `GET /agent`) includes
/// a first-class `plan` agent whose description is literally
/// `"Plan mode. Disallows all edit tools."`. It owns the plan-vs-act
/// contract end-to-end (system prompt, tool whitelist, permission
/// defaults), so the cleanest mapping is:
///
/// - `PermissionMode::Plan` → `agent: "plan"`
/// - everything else         → omit (opencode defaults to `build`)
///
/// The session-level `permission` ruleset still applies on top, as
/// defence-in-depth for the non-plan modes.
///
/// Verified via probe: unknown agent names are silently ignored by
/// opencode (fall back to `build`), so the worst case of a stale
/// agent name here is a degraded UX, not an error response.
fn agent_for(mode: PermissionMode) -> Option<&'static str> {
    match mode {
        PermissionMode::Plan => Some("plan"),
        _ => None,
    }
}

/// Map flowstate's `ReasoningEffort` onto opencode's variant names.
/// Opencode variants are per-model and typically expose the keys
/// `"low" | "medium" | "high" | "xhigh" | "max"`. Flowstate's
/// `Minimal` has no direct analogue — we collapse it onto `"low"` so
/// it still picks the lightest available reasoning budget.
fn effort_to_variant(effort: ReasoningEffort) -> Option<&'static str> {
    Some(match effort {
        ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    })
}

/// Detect whether a model is actually free to call.
///
/// This is *not* simply "cost.input == 0 && cost.output == 0". Live
/// probe against opencode 1.4.3 (see `tests/live_opencode.rs` and
/// `/tmp/opencode-probe/probe2.mjs`) found that other providers
/// routinely report zero cost too:
///
/// - `openai`, `github-copilot` — every model returned with
///   `cost: {input: 0, output: 0}`, because the installation had no
///   API key configured and opencode strips pricing for the
///   unauthenticated catalog view.
/// - `zai-coding-plan` — 12 of 13 models at zero cost because Z.AI
///   bills under a flat subscription, so *per-request* cost really
///   is zero even though accessing the models requires a paid plan.
/// - `opencode` (Zen) — only 4 of 35 models at zero cost, matching
///   Zen's actual free tier (e.g. `minimax-m2.5-free`, `big-pickle`,
///   `gpt-5-nano`). These are the ones we want to badge.
///
/// So we gate the badge on `provider_id == "opencode"`: that's the
/// one provider where zero cost is a reliable signal of "the user
/// can call this without additional billing or configuration".
fn is_free_model(provider_id: &str, model: &Value) -> bool {
    if provider_id != "opencode" {
        return false;
    }
    let cost = match model.get("cost") {
        Some(Value::Object(_)) => model.get("cost").unwrap(),
        _ => return false,
    };
    let input = cost.get("input").and_then(Value::as_f64);
    let output = cost.get("output").and_then(Value::as_f64);
    matches!((input, output), (Some(0.0), Some(0.0)))
}

/// Translate flowstate's `"provider/model"` slug into opencode's
/// `{ providerID, modelID }` object.
///
/// Opencode's REST server rejects a bare `"model": "openai/gpt-5"`
/// string field with a 400; the schema demands the split object form.
/// We keep the flat slug as the canonical representation inside
/// flowstate (one column in the session row, one dropdown value in
/// the UI) and only materialise the object at the wire boundary.
///
/// Returns `None` when the input doesn't contain a `/` — callers
/// should omit the field in that case so opencode falls back to its
/// configured default model instead of rejecting the whole request.
fn parse_model_slug(slug: &str) -> Option<Value> {
    let (provider_id, model_id) = slug.split_once('/')?;
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    Some(json!({
        "providerID": provider_id,
        "modelID": model_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_slug_splits_provider_and_model() {
        let parsed = parse_model_slug("openai/gpt-5").unwrap();
        assert_eq!(parsed["providerID"], "openai");
        assert_eq!(parsed["modelID"], "gpt-5");
    }

    #[test]
    fn parse_model_slug_allows_slashes_inside_model_id() {
        // Opencode has model ids with slashes in them (e.g. community
        // providers proxying HuggingFace paths). `split_once` only
        // splits on the first `/`, so the tail stays intact.
        let parsed = parse_model_slug("vendor/family/variant").unwrap();
        assert_eq!(parsed["providerID"], "vendor");
        assert_eq!(parsed["modelID"], "family/variant");
    }

    #[test]
    fn parse_model_slug_returns_none_without_separator() {
        assert!(parse_model_slug("gpt-5").is_none());
    }

    #[test]
    fn parse_model_slug_returns_none_on_empty_side() {
        assert!(parse_model_slug("/gpt-5").is_none());
        assert!(parse_model_slug("openai/").is_none());
    }

    #[test]
    fn is_free_model_flags_opencode_zero_cost() {
        let model = json!({
            "id": "gpt-5-nano",
            "cost": { "input": 0, "output": 0 }
        });
        assert!(is_free_model("opencode", &model));
    }

    #[test]
    fn is_free_model_ignores_openai_zero_cost_reflections() {
        // Live probe showed openai returns cost=0 for every model
        // when the installation has no OpenAI key configured —
        // that's a catalog reflection, not a free tier.
        let model = json!({
            "id": "gpt-5.1-codex-max",
            "cost": { "input": 0, "output": 0 }
        });
        assert!(!is_free_model("openai", &model));
    }

    #[test]
    fn is_free_model_ignores_zai_coding_plan_zero_cost() {
        // Z.AI bills a flat subscription; per-request cost is zero
        // but we don't want to imply the plan itself is free.
        let model = json!({
            "id": "glm-5",
            "cost": { "input": 0, "output": 0 }
        });
        assert!(!is_free_model("zai-coding-plan", &model));
    }

    #[test]
    fn is_free_model_ignores_opencode_paid_zen() {
        let model = json!({
            "id": "gpt-5.1-codex-max",
            "cost": { "input": 1.25, "output": 10 }
        });
        assert!(!is_free_model("opencode", &model));
    }

    #[test]
    fn is_free_model_ignores_missing_cost() {
        let model = json!({ "id": "something" });
        assert!(!is_free_model("opencode", &model));
    }

    #[test]
    fn agent_for_plan_picks_plan_agent() {
        assert_eq!(agent_for(PermissionMode::Plan), Some("plan"));
    }

    #[test]
    fn agent_for_non_plan_omits_field() {
        // Letting opencode fall through to its default `build` agent
        // is intentional — live probe showed `build` is the one
        // with the "allow everything, ask on doom_loop" contract
        // flowstate's non-plan modes want.
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Auto,
        ] {
            assert_eq!(agent_for(mode), None, "{mode:?} should omit agent");
        }
    }

    #[test]
    fn effort_to_variant_maps_minimal_to_low() {
        assert_eq!(effort_to_variant(ReasoningEffort::Minimal), Some("low"));
        assert_eq!(effort_to_variant(ReasoningEffort::Low), Some("low"));
        assert_eq!(effort_to_variant(ReasoningEffort::Medium), Some("medium"));
        assert_eq!(effort_to_variant(ReasoningEffort::High), Some("high"));
        assert_eq!(effort_to_variant(ReasoningEffort::Xhigh), Some("xhigh"));
        assert_eq!(effort_to_variant(ReasoningEffort::Max), Some("max"));
    }

    #[test]
    fn permission_rules_bypass_allows_everything() {
        let rules = permission_rules_for(PermissionMode::Bypass);
        let arr = rules.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["permission"], "*");
        assert_eq!(arr[0]["action"], "allow");
    }

    #[test]
    fn variant_keys_extracts_in_intensity_order() {
        // Real fixture copied from `/config/providers` for
        // `opencode/gpt-5.1-codex-max`.
        let model = json!({
            "variants": {
                "high":   { "reasoningEffort": "high" },
                "low":    { "reasoningEffort": "low" },
                "medium": { "reasoningEffort": "medium" },
            }
        });
        assert_eq!(
            variant_keys(&model),
            vec!["low", "medium", "high"],
            "expected canonical ascending order regardless of JSON order"
        );
    }

    #[test]
    fn variant_keys_filters_unknown_variants() {
        let model = json!({
            "variants": {
                "low": {},
                "codex-low": {},
                "high": {},
                "banana": {}
            }
        });
        assert_eq!(variant_keys(&model), vec!["low", "high"]);
    }

    #[test]
    fn variant_keys_empty_when_no_variants_object() {
        assert_eq!(variant_keys(&json!({ "id": "kimi" })), Vec::<String>::new());
    }

    #[test]
    fn variant_keys_empty_when_variants_is_not_object() {
        assert_eq!(variant_keys(&json!({ "variants": [] })), Vec::<String>::new());
    }

    #[test]
    fn permission_rules_default_always_allows_question() {
        // Denying `question` hangs the ask-user tool flow; this
        // invariant must hold for every non-bypass mode.
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            let rules = permission_rules_for(mode);
            let question_rule = rules
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["permission"] == "question")
                .unwrap_or_else(|| panic!("{mode:?} missing `question` rule"));
            assert_eq!(
                question_rule["action"], "allow",
                "{mode:?} must allow `question`"
            );
        }
    }
}
