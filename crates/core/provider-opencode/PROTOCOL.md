# opencode wire protocol reference

The opencode server protocol isn't fully documented outside opencode's own
TypeScript SDK. This file is the accumulated reverse-engineering notes from
driving a real `opencode serve` subprocess — every shape here was confirmed
against opencode **`1.14.41`** (re-baselined from `1.4.3`) via the probe
scripts at `/tmp/opencode-probe/` and the live `GET /doc` introspection
endpoint described in the next section.

Keep this in lockstep with the adapter code. When opencode ships a new event
type or renames a field:

1. Diff `GET /doc` against the committed snapshot under
   `tests/fixtures/openapi-<version>.json` (see ["Live introspection"](#live-introspection)).
2. Reproduce any behavioural change with a one-off probe in
   `/tmp/opencode-probe/`.
3. Update the fixture-based unit tests in `src/events.rs::tests` and
   `src/http.rs::tests`.
4. Update the relevant section of this file.

The gated live smoke test (`tests/live_opencode.rs`, run with
`cargo test --test live_opencode -- --ignored`) is the last line of defence
against silent drift.

---

## Live introspection (`GET /doc`)

`GET /doc` on a running `opencode serve` returns the OpenAPI **3.1.1** JSON
document for the embedded HTTP API.

```
GET /doc
→ 200 application/json
{
  "openapi": "3.1.1",
  "info":  { "title": "opencode", "description": "opencode api", "version": "0.0.3" },
  "paths": { "/auth/{providerID}": {...}, "/log": {...} },
  "components": { "schemas": { "ApiAuth", "Auth", "BadRequestError", "OAuth", "WellKnownAuth" } }
}
```

Two important caveats discovered during the 1.14.41 re-baseline:

- **`/doc` is partial.** As of 1.14.41 it advertises only `auth.set`,
  `auth.remove`, and `app.log`. Every endpoint flowstate actually depends on
  (`/session`, `/session/{id}/prompt_async`, `/event`, `/agent`,
  `/config/providers`, `/session/{id}/permissions/{permID}`,
  `/question/{id}/reply`) is **not** in the spec — those are effectively
  un-schematized internal endpoints. Treat `/doc` as a lower-bound contract,
  not a complete one.
- **`info.version` is the spec version, not the binary version.** Currently
  `0.0.3`; the binary version is reported separately by
  `POST /session` (see below) and by `opencode --version`.

Aliases that might look promising (`/openapi.json`, `/swagger.json`,
`/spec.json`, `/scalar`, `/reference`, `/docs`, `/openapi.yaml`) all 200 with
the SPA shell HTML — they are caught by the web-UI fallback route, not real
spec endpoints. **`/doc` is the only working spec URL.**

The committed fixture at
`tests/fixtures/openapi-1.14.41.json` is the snapshot for the current baseline;
re-curl it on every upgrade and `git diff` to see what shifted.

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
| `POST` | `/session` | Create a new opencode session. | `{ directory, model?: {id, providerID, modelID?}, permission? }` + **`x-opencode-directory: <urlencoded>` header** required for tool dispatch. | `200 OK` JSON `{ id, slug, version, projectID, directory, title, time: { created, updated } }`. The `version` field is the **opencode binary version** (e.g. `"1.14.41"`) — usable for runtime compatibility checks without a separate `opencode --version` shell-out. |
| `PATCH` | `/session/{id}` | Update a session's persistent fields. **Only `permission` actually persists** — see "Updating session permissions" below. Other fields (`model`, `variant`, `agent`) are silently ignored: opencode returns `200 OK` and the session row is unchanged. | `{ permission?: [...] }` | `200 OK` JSON — the (possibly-unchanged) session row. |
| `POST` | `/session/{id}/prompt_async` | Enqueue a user prompt. All streaming arrives via SSE. **`x-opencode-directory: <urlencoded>` header is required** — without it tools run in `process.cwd()` of the opencode server. | `{ parts: [{type: "text", text}], model?: {providerID, modelID, id?}, variant?, agent? }` | `204 No Content` (or `200`) |
| `GET` | `/agent` | List available opencode agents (one entry per built-in agent). | — | `200 OK` JSON array |
| `POST` | `/session/{id}/abort` | Interrupt an in-flight turn. Idempotent. | `{}` | `200 OK` or `204` |
| `POST` | `/session/{id}/permissions/{permissionID}` | Answer a pending `permission.asked` event. | `{ response: "once" \| "always" \| "reject" }` — body field is **`response`**, not `reply`. Opencode 1.14.41 schema-validates against this exact key; sending `{"reply": …}` returns 400 and the tool that asked for the prompt stays wedged at `pending` forever. | `200 OK` body literal `true` |
| `POST` | `/question/{id}/reply` | Answer a pending `question.asked` event. Note the lack of `/session/` prefix — opencode's question endpoints are session-agnostic. | `{ requestID, answers: [[label, ...], ...] }` (one inner array per question the event carried) | `200 OK` |
| `GET` | `/event` | Subscribe to the SSE event stream. **Scoped by `x-opencode-directory` header** — opencode 1.14.41 only delivers events from sessions whose directory matches the subscribing request. One reader per project directory; multiple concurrent SSE clients on a single server work fine. | — | `text/event-stream` |

### `x-opencode-directory` — the per-request working-directory header

Opencode 1.14.41 resolves the per-request working directory via a server-side
middleware that reads, in order:

1. `?directory=<urlencoded-path>` query parameter
2. `x-opencode-directory: <urlencoded-path>` HTTP header
3. fallback: `process.cwd()` of the opencode server process

The encoding is JS `encodeURIComponent` semantics — every byte outside the
unreserved set `[A-Za-z0-9-._~]` is `%XX`-escaped, slashes included. The
adapter has a small helper `urlencode_path` that produces this exact form.

**The header is required on every per-session HTTP call**, including:
- `POST /session` (otherwise tools dispatched during the session-init step
  hit the wrong cwd; opencode treats the body's `directory` as session
  metadata only).
- `POST /session/{id}/prompt_async` (the critical one — without it the
  bash tool runs in `process.cwd()` of the server).
- `PATCH /session/{id}`
- `POST /session/{id}/abort`
- `POST /session/{id}/permissions/{permID}`
- `POST /question/{id}/reply`
- `GET /event` (otherwise the SSE stream silently drops every event from
  sessions whose directory differs from the server's CWD; this was the
  root cause of "the prompt 204s but `wait_for_completion` times out").

Endpoints that are server-scoped (no session): `GET /app`,
`GET /config/providers`. They don't need the header.

The official `@opencode-ai/sdk` injects `x-opencode-directory` automatically
when constructed with `{ directory: "..." }`. Our hand-rolled Rust HTTP
client opts in explicitly via the `directory` argument on each
`request_get` / `request_post` / `request_patch` helper.

### Sending a prompt

```json
POST /session/ses_abc/prompt_async
{
  "model": { "id": "opencode/kimi-k2.5", "providerID": "opencode", "modelID": "kimi-k2.5" },
  "variant": "medium",
  "parts": [{ "type": "text", "text": "…" }]
}
```

- **`model` MUST be an object.** A bare string `"opencode/kimi-k2.5"`
  returns `400 Bad Request` with `expected object, received string`. This
  is the #1 shape-drift bug we've hit; the adapter's `parse_model_slug`
  helper splits `provider/model` into the object form.
- **The two model-bearing endpoints validate against different
  shapes.** `POST /session` requires `{ id, providerID }` and `POST
  /session/{id}/prompt_async` requires `{ providerID, modelID }`.
  Both endpoints silently accept the union `{ id, providerID, modelID }`,
  so `parse_model_slug` emits all three keys and one helper feeds both
  call-sites. Sending the wrong subset to an endpoint returns
  `400 Bad Request` with `expected string, path: ["model","<missing>"]`.
  `id` is the flat slug (e.g. `"opencode/kimi-k2.5"`); the server
  echoes back `{ id, providerID }` on session reads.
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
  "model": { "id": "opencode/kimi-k2.5", "providerID": "opencode", "modelID": "kimi-k2.5" },
  "permission": [
    { "permission": "bash",     "pattern": "*", "action": "ask"   },
    { "permission": "edit",     "pattern": "*", "action": "ask"   },
    { "permission": "question", "pattern": "*", "action": "allow" }
  ]
}
```

### Updating session permissions

Opencode persists the permission ruleset on the session row — every
subsequent tool invocation consults that stored ruleset, *not* anything
sent on `prompt_async`. So a session minted with one mode keeps that
mode forever unless the row is rewritten.

That's a problem in flowstate: `start_session` mints the native session
before the user has picked a permission mode (the runtime only hands
that through on `execute_turn`), and the user can switch modes between
turns on a long-lived session. Both cases need an in-place rewrite.

```json
PATCH /session/ses_abc
{
  "permission": [
    { "permission": "*", "pattern": "*", "action": "allow" }
  ]
}
```

- Returns `200 OK` with the updated session row.
- Discovered by probe — not in `/doc`. Other plausible shapes
  (`POST /session/{id}/permission`, `PUT /session/{id}/permission`,
  `POST /session/{id}/permissions`) all fall through to the SPA HTML
  404. `PATCH /session/{id}` is the only one that actually rewrites.
- Effect is immediate. Live probe (1.14.41): a session created with
  `bash: ask`, then PATCHed to `*: allow`, runs the next bash tool to
  completion without firing `permission.asked`.
- The adapter calls this once per `execute_turn` to reconcile the
  stored ruleset with the caller's `PermissionMode`. Failure aborts the
  turn — running with stale rules could either wedge the user (asks
  when they wanted bypass) or silently skip prompting (allows when they
  wanted ask), so we treat it as a hard precondition.

### What PATCH does *not* do

The same endpoint silently accepts (and silently ignores) `model`,
`variant`, and `agent`. Live probe (1.14.41):

```bash
# session was created with model=opencode/gpt-5-nano
curl -X PATCH /session/ses_xxx -d '{"model":{"id":"opencode/kimi-k2.5",...}}'
#  → 200 OK, response body still shows model = gpt-5-nano
curl /session/ses_xxx
#  → model: { id: "opencode/gpt-5-nano", providerID: "opencode" }
```

So **don't try to stash the model or reasoning effort on the session
row** — they have to ride on `POST /session/{id}/prompt_async` every
turn. There's a related quirk: the session's persisted `model` field
is also *not consulted* by `prompt_async`. A prompt sent without a
`model` field falls back to opencode's **global server default** (in
the wild: `openai/gpt-5.4`), regardless of what the session was
created with. So the per-prompt `model` field is mandatory in
practice — the session row's copy is effectively decorative.

This shape difference is why the adapter:

- PATCHes only `permission` per turn (in `OpenCodeClient::update_permission`),
  because that's the field that genuinely needs cross-turn persistence.
- Re-sends `model`, `variant`, and `agent` on every `prompt_async`
  body, because opencode treats those as per-turn parameters.

Known permission categories — the set has expanded since 1.4.3.
Live-confirmed against 1.14.41 by enumerating `permission.permission` across
every entry returned by `GET /agent`:

| Category | Notes |
|----------|-------|
| `*` | Wildcard — any category. Matches first. |
| `bash` | Shell command execution. |
| `edit` | File modification (write/patch tools). |
| `read` | **New in 1.14.x.** File read. The built-in agents allow `read` by default; flowstate previously didn't express read intent at all. |
| `grep` | **New in 1.14.x.** Codebase regex search. Replaces the role of `codesearch`. |
| `glob` | **New in 1.14.x.** Filename pattern search. |
| `list` | **New in 1.14.x.** Directory listing. |
| `todowrite` | **New in 1.14.x.** Built-in todo-list tool. |
| `webfetch` | HTTP fetch of arbitrary URLs. |
| `websearch` | Web search tool. |
| `external_directory` | File access outside the session's `directory` root. |
| `doom_loop` | Loop-detection guard — deny to break runaway tool loops. |
| `plan_enter` | **New in 1.14.x.** Agent transitioning into plan mode. |
| `plan_exit` | **New in 1.14.x.** Agent leaving plan mode. |
| `question` | Built-in ask-user tool. **Must always be `allow`** (see below). |
| `codesearch` | **Legacy.** Removed from built-in agent definitions in 1.14.x. The server still **accepts** sessions that send `codesearch` rules with `200 OK`, so we keep emitting it for back-compat with older opencode installs while migrating call-sites to `grep`. |

Actions: `allow`, `ask`, `deny`.

**`question` should always be `allow`** unless you actively want to break
the agent's ask-user flow — a `deny` or missing rule causes opencode to
silently hang when the model tries to ask a clarifying question.

Our `PermissionMode → ruleset` mapping lives in
`permission_rules_for()` in `http.rs`.

---

## Agents (`GET /agent`)

Opencode ships a small registry of built-in agents. Probe-captured
list as of `1.14.41` (unchanged from `1.4.3`):

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
- OpenAPI schema (live): `GET /doc` on the running server returns the raw
  OpenAPI 3.1.1 JSON directly — see ["Live introspection"](#live-introspection)
  above. (No HTML swagger wrapper; `/scalar`, `/reference`, `/openapi.json`,
  etc. are SPA-shell fallbacks, not real endpoints.) Note the spec is
  **partial** — it covers `auth.set`, `auth.remove`, `app.log` only as of
  1.14.41; the session/event/permission/agent endpoints are
  un-schematized and discovered empirically.
- Generated TypeScript SDK (mirrors the same partial spec):
  <https://www.npmjs.com/package/@opencode-ai/sdk>
