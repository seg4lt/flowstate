# opencode wire protocol reference

The opencode server protocol isn't formally documented outside opencode's own
TypeScript SDK. This file is the accumulated reverse-engineering notes from
driving a real `opencode serve` subprocess — every shape here was confirmed
against opencode `1.4.3` via the probe scripts at `/tmp/opencode-probe/`.

Keep this in lockstep with the adapter code. When opencode ships a new event
type or renames a field:

1. Reproduce the change with a one-off probe in `/tmp/opencode-probe/`.
2. Update the fixture-based unit tests in `src/events.rs::tests` and
   `src/http.rs::tests`.
3. Update the relevant section of this file.

The gated live smoke test (`tests/live_opencode.rs`, run with
`cargo test --test live_opencode -- --ignored`) is the last line of defence
against silent drift.

---

## Server lifecycle

### Starting the server

```
opencode serve --hostname 127.0.0.1 --port <PORT>
```

- **Readiness signal**: the line `opencode server listening on http://...`
  lands on **stdout** (not stderr).
- **Auth**: basic auth, username `opencode` (overridable via
  `OPENCODE_SERVER_USERNAME`), password read from
  `OPENCODE_SERVER_PASSWORD` env var. We generate a 32-byte hex password
  per spawn and pass it on stdin; stale processes from crashed daemons
  can't answer for us.
- **Multi-session**: one server multiplexes all opencode sessions. We
  spawn exactly one per flowstate daemon, lazily on first use
  (`tokio::sync::OnceCell` in `OpenCodeAdapter::ensure_server`).

### Shutdown

Dropping `OpenCodeServer` sends SIGTERM via `kill_on_drop(true)` and an
explicit `start_kill()` through the child-handle mutex. No graceful-exit
handshake — opencode doesn't expose one.

---

## REST endpoints

All requests require basic auth.

| Method | Path | Purpose | Request body | Response |
|--------|------|---------|--------------|----------|
| `GET` | `/app` | Liveness probe. Returns the web-UI HTML, but a 2xx is the only signal we need. | — | `200 OK` (HTML) |
| `GET` | `/config/providers` | Model catalogue (providers × models + capabilities + costs). | — | `200 OK` JSON — see "Model catalogue shape" below |
| `POST` | `/session` | Create a new opencode session. | `{ directory, model?, permission? }` | `200 OK` JSON `{ id, slug, version, projectID, directory, title, time: { created, updated } }` |
| `POST` | `/session/{id}/prompt_async` | Enqueue a user prompt. All streaming arrives via SSE. | `{ parts: [{type: "text", text}], model?: {providerID, modelID}, variant?, agent? }` | `204 No Content` (or `200`) |
| `GET` | `/agent` | List available opencode agents (one entry per built-in agent). | — | `200 OK` JSON array |
| `POST` | `/session/{id}/abort` | Interrupt an in-flight turn. Idempotent. | `{}` | `200 OK` or `204` |
| `POST` | `/session/{id}/permissions/{permissionID}` | Answer a pending `permission.asked` event. | `{ reply: "once" \| "always" \| "reject" }` | `200 OK` |
| `POST` | `/question/{id}/reply` | Answer a pending `question.asked` event. Note the lack of `/session/` prefix — opencode's question endpoints are session-agnostic. | `{ requestID, answers: [[label, ...], ...] }` (one inner array per question the event carried) | `200 OK` |
| `GET` | `/event` | Subscribe to the SSE event stream. One long-lived connection per server, demultiplexed per session by the payload's `sessionID`. | — | `text/event-stream` |

### Sending a prompt

```json
POST /session/ses_abc/prompt_async
{
  "model": { "providerID": "opencode", "modelID": "kimi-k2.5" },
  "variant": "medium",
  "parts": [{ "type": "text", "text": "…" }]
}
```

- **`model` MUST be an object.** A bare string `"opencode/kimi-k2.5"`
  returns `400 Bad Request` with `expected object, received string`. This
  is the #1 shape-drift bug we've hit; the adapter's `parse_model_slug`
  helper splits `provider/model` into the object form.
- **`variant` is optional and silently ignored for unknown values.** Probe
  confirmed both `"nonsense"` and legitimate `"low"/"medium"/"high"/
  "xhigh"/"max"` return `204`. We always send one when the caller
  provides a `ReasoningEffort`; no per-model variant catalogue lookup
  needed.
- **`agent` is optional — this is how you flip plan-vs-act.** Opencode's
  agent registry is session-agnostic and owns its own plan/edit
  contract; `"plan"` disallows edit tools itself and our adapter uses
  it for `PermissionMode::Plan`. Unknown names silently fall back to
  `build` (see `/agent` below for the live catalogue). When omitted,
  opencode uses `build` (the default).

### Creating a session with a permission ruleset

```json
POST /session
{
  "directory": "/path/to/project",
  "model": { "providerID": "opencode", "modelID": "kimi-k2.5" },
  "permission": [
    { "permission": "bash",     "pattern": "*", "action": "ask"   },
    { "permission": "edit",     "pattern": "*", "action": "ask"   },
    { "permission": "question", "pattern": "*", "action": "allow" }
  ]
}
```

Known permission categories (live-confirmed): `bash`, `edit`, `webfetch`,
`websearch`, `codesearch`, `external_directory`, `doom_loop`, `question`.
`*` matches any category.

Actions: `allow`, `ask`, `deny`.

**`question` should always be `allow`** unless you actively want to break
the agent's ask-user flow — a `deny` or missing rule causes opencode to
silently hang when the model tries to ask a clarifying question.

Our `PermissionMode → ruleset` mapping lives in
`permission_rules_for()` in `http.rs`.

---

## Agents (`GET /agent`)

Opencode ships a small registry of built-in agents. Probe-captured
list as of `1.4.3`:

| Name | Description |
|------|-------------|
| `build` | The default agent. Executes tools based on configured permissions. |
| `plan` | Plan mode. Disallows all edit tools. |
| `explore` | Fast agent specialized for exploring codebases (quick / medium / very thorough). |
| `general` | General-purpose agent for researching complex questions and executing multi-step tasks in parallel. |
| `compaction` | (internal — conversation summarisation) |
| `summary` | (internal) |
| `title` | (internal — generates session titles) |

The primary lever for us is `build` vs `plan`. The adapter's
`agent_for(PermissionMode)` maps:

| `PermissionMode` | `agent` field sent |
|------------------|-------------------|
| `Plan` | `"plan"` |
| `Default` / `AcceptEdits` / `Bypass` / `Auto` | omitted (opencode uses `build`) |

The `plan` agent handles the read-only contract itself, so our
session-level `permission` ruleset for Plan is intentionally minimal
(allows reads, doesn't restate the edit denials the agent already
enforces). For every other mode, session-level `permission` is the
primary gate.

---

## Model catalogue shape

`GET /config/providers` returns:

```json
{
  "providers": [
    {
      "id": "opencode",
      "name": "OpenCode Zen",
      "models": {
        "kimi-k2.5": {
          "id": "kimi-k2.5",
          "name": "Kimi K2.5",
          "providerID": "opencode",
          "api": { "url": "https://opencode.ai/zen/v1", "npm": "@ai-sdk/..." },
          "cost": { "input": 0, "output": 0, "cache": { "read": 0, "write": 0 } },
          "limit": { "context": 200000, "output": 65536 },
          "capabilities": { "reasoning": true, "toolcall": true, … },
          "variants": { "low": {…}, "medium": {…}, "high": {…} }
        }
      }
    }
  ]
}
```

(Historical shape: `models` has been both an object keyed by id and an
array of `{id, name, …}` entries. Our parser accepts both.)

### "Free" tag heuristic

**Not every model with `cost: {input: 0, output: 0}` is free.** Live
probe findings:

| Provider | Zero-cost entries | Actually free? |
|----------|-------------------|----------------|
| `opencode` (Zen) | 4 of 35 (`minimax-m2.5-free`, `big-pickle`, `gpt-5-nano`, …) | **yes** — Zen free tier |
| `openai`, `github-copilot` | every entry | **no** — unauthenticated catalogue reflection |
| `zai-coding-plan` | 12 of 13 | **no** — flat subscription, not free per call |
| `ollama` | all | **no** — the user runs them locally |

Our `is_free_model` helper gates the badge on `provider_id == "opencode"
&& cost.input == 0 && cost.output == 0`.

---

## SSE event stream

`GET /event` emits a long-lived SSE connection. Each frame is a single
`data:` payload of JSON. Every session-scoped event includes a
`sessionID` somewhere in its `properties`; we use it to route events to
the right flowstate session's `TurnEventSink`.

### Session id location by event

Opencode puts `sessionID` in different places depending on event:

- `properties.sessionID` — most events
- `properties.info.sessionID` — `message.updated`
- `properties.part.sessionID` — `message.part.updated`

Our `find_session_id` helper walks all four known paths.

### Event inventory

What we've observed on a live server, grouped by how the adapter treats
each one:

#### Drive turn output

- **`message.part.delta`** — THE text streaming event.
  ```json
  { "type": "message.part.delta",
    "properties": { "sessionID": "ses_x", "messageID": "msg_x",
                    "partID": "prt_x", "field": "text",
                    "delta": " Hello" } }
  ```
  `delta` is **already incremental** — a proper delta, not a snapshot.
  Emit straight through to the sink.

- **`message.part.updated`** with `part.type === "tool"` — tool-call
  lifecycle.
  ```json
  { "type": "message.part.updated",
    "properties": { "sessionID": "ses_x",
                    "part": { "type": "tool", "callID": "c_1",
                              "tool": "bash",
                              "state": { "status": "pending"|"running"|"completed"|"error",
                                         "input": {…}, "output": "...", "error": "..." } } } }
  ```
  Dedupe: opencode fires this many times per tool as it progresses.
  Emit `ToolCallStarted` on the first `pending`/`running` we see for a
  given `callID`, then `ToolCallCompleted` on `completed`/`error`.

- **`message.part.updated`** with `part.type === "text" | "reasoning"` —
  snapshot of the assistant's current text. **Ignore** — we're already
  streaming via `message.part.delta`; emitting again would double-print.

#### Turn lifecycle

- **`session.idle`** — the real end-of-turn signal.
  ```json
  { "type": "session.idle", "properties": { "sessionID": "ses_x" } }
  ```
  **Not** `session.status` with `type: "idle"` — that's a separate
  event that never appears. This was the #2 bug we hit early.

- **`session.status`** — coarse status.
  ```json
  { "type": "session.status",
    "properties": { "sessionID": "ses_x",
                    "status": { "type": "busy"|"retry", "message": "…" } } }
  ```
  `idle` type is NOT emitted here (live probe confirmed: only `busy`
  and `retry`). We ignore `busy` and forward `retry` as an Info event.

- **`session.error`** — turn failed.
  ```json
  { "type": "session.error",
    "properties": { "sessionID": "ses_x",
                    "error": { "message": "…", "name": "…" } } }
  ```
  Resolve the subscription oneshot with `Err`.

#### Prompts to the user

- **`permission.asked`** — agent wants tool permission.
  ```json
  { "type": "permission.asked",
    "properties": { "id": "perm_x", "sessionID": "ses_x",
                    "permission": "bash",
                    "patterns": ["rm -rf /tmp/foo"],
                    "metadata": { … tool args … } } }
  ```
  Answer with `POST /session/{sid}/permissions/{perm_x}`
  body `{"reply": "once"|"always"|"reject"}`.

- **`question.asked`** — agent calls its ask-user tool.
  ```json
  { "type": "question.asked",
    "properties": { "id": "que_x", "sessionID": "ses_x",
                    "questions": [ { "question": "…", "header": "…",
                                     "options": [ { "label": "Yes",
                                                    "description": "…" } ] } ],
                    "tool": { "messageID": "…", "callID": "functions.question:0" } } }
  ```
  Answer with `POST /question/{que_x}/reply`
  body `{"requestID": "que_x", "answers": [["Yes"]]}`
  (one inner array per question, each holding chosen option labels).

- **`permission.replied`**, **`question.replied`** — opencode echoes the
  answer back after our POST. Informational, ignored.

#### Known-ignored lifecycle events

`message.updated`, `message.removed`, `session.updated`, `session.diff`,
`session.started`, `session.exited`, `server.connected`,
`server.heartbeat`, `question.rejected`.

Unknown event types degrade to a `debug!` log and continue — forward-
compatible when opencode ships new events.

---

## How to capture new shapes

Every finding in this doc came from one of the scripts under
`/tmp/opencode-probe/` (regenerate them locally if needed; they're
disposable). Pattern:

1. Spawn opencode yourself:

   ```js
   const proc = spawn("opencode",
     ["serve", "--hostname", "127.0.0.1", "--port", String(port)],
     { env: { ...process.env, OPENCODE_SERVER_PASSWORD: pass } });
   ```

2. Wait for `listening` on stdout.
3. Hit whatever endpoint you want to characterise. Log responses.
4. Subscribe to `/event` BEFORE firing a prompt so you don't miss any
   events.
5. Dump the first occurrence of each event `type` — that's usually
   enough to infer the schema.

See existing probes:

- `probe.mjs` — baseline happy path; captured `message.part.delta`,
  `session.idle`, `session.status`.
- `probe2.mjs` — `/config/providers` catalogue shapes + forcing
  `question.asked`.
- `probe3.mjs` — question-reply endpoint discovery (tried multiple paths,
  found `POST /question/{id}/reply`).
- `probe4.mjs` — permission-ruleset behaviour verification.
- `probe5.mjs` — quick shape-acceptance for permission rulesets +
  variant strings.
- `probe6.mjs` — agent discovery via `GET /agent`; verified
  `agent: "plan"` is accepted and reflected back on subsequent
  `message.updated` events.

---

## Running the adapter's tests

| Target | Command | Time |
|--------|---------|------|
| All in-crate unit tests (no network, canned fixtures) | `cargo test -p zenui-provider-opencode` | ~60 ms |
| End-to-end live smoke (spawns real opencode, hits Zen) | `cargo test -p zenui-provider-opencode --test live_opencode -- --ignored --nocapture` | ~10 s |

The live test requires `opencode` on PATH and network access to
`https://opencode.ai/zen/v1`. It auto-skips when the binary is missing.

---

## Upstream references

- Opencode source and docs: <https://github.com/sst/opencode> and
  <https://opencode.ai/docs/>
- Server mode overview: <https://opencode.ai/docs/server>
- Models catalogue (source of truth for cost data):
  <https://models.dev/api.json>
- OpenAPI schema (live): `GET /doc` on the running server (returns an
  HTML swagger UI; the raw JSON schema lives behind that).
