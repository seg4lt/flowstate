# persistence — boundary rules

This crate is the SDK's on-disk state. It stores **only what the agent
runtime needs to execute or resume agents**. Everything a user
perceives as a display label — titles, project names, previews, list
ordering, archive filters — belongs to the consuming app and is
tracked in that app's own store, never here.

## Litmus test for new fields

Before adding a column, field on `ProjectRecord` / `SessionSummary`,
or any new persisted type, ask:

> Does any code in `runtime-core`, `orchestration`, `daemon-core`, or
> a provider adapter read this field to make a decision?

- **Yes** → it's a runtime concern, persist it here.
- **No** → it's a display concern. Put it in the consuming app's own
  store (for flowstate, that's the `session_display` / `project_display`
  tables in `flowstate/src-tauri/src/user_config.rs`).

"It's convenient for the UI to read it here" is not the runtime
reading it. Convenience is a bridge between storage and UI that the
app owns, not the SDK.

## What currently lives here (and why)

| Table / field | Why it's runtime-essential |
|---|---|
| `sessions` core (`session_id`, `provider`, `status`, `created_at`, `updated_at`, `turn_count`, `provider_state_json`, `model`, `project_id`) | Session lifecycle state the runtime reads on every resume, turn routing, and project lookup. |
| `turns` (all `*_json` blobs: `reasoning`, `tool_calls`, `file_changes`, `subagents`, `plan`, `blocks`, `permission_mode`, `reasoning_effort`) | Full turn reconstruction on session load. Providers that resume mid-conversation need this verbatim. |
| `turn_attachments` | Metadata for attachment files under `<data_dir>/attachments/`; runtime reloads them on session restore. |
| `projects.project_id`, `projects.path` | `runtime-core::get_project()` reads `path` to set `session.cwd` at session start. Without it, the agent runs from the wrong working directory. |
| `projects.deleted_at` | Internal soft-delete filter for the resurrection flow (recreating a project with the same `path` un-tombstones the existing row so its sessions reattach). Never surfaced to the app. |
| `provider_enablement` | Runtime gate: `runtime-core` refuses health checks, model fetches, and turn routing for disabled providers. Not a UI toggle — the app only reflects the enforced state. |
| `provider_model_cache` / `provider_health_cache` | Consulted at daemon bootstrap and on every health check to avoid re-probing CLIs. Startup-path optimization owned by the runtime. |
| `archived_sessions` / `archived_turns` | Session lifecycle state machine: `archive_session` / `unarchive_session` physically move rows between live and archive tables under runtime control. |

## What deliberately does NOT live here

| Field | Lives in | Why |
|---|---|---|
| Session title | App store (flowstate: `session_display.title`) | Never read by the runtime. Apps derive titles client-side (e.g. first 6 words of input) or let the user set one. |
| Session last-turn preview | App store (same tables) | Display affordance for session lists. Apps can derive it from the last turn's `output` — no need for a persisted column. |
| Project name | App store (flowstate: `project_display.name`) | User-visible label. Runtime only needs `project_id` and `path`. |
| Project sort order | App store | Arbitrary UI ordering preference. Different apps sort differently (alphabetically, by recency, etc.). |

## Wire protocol implications

Because these fields are out of scope here, they are also out of scope
for `provider-api` types (`SessionSummary`, `ProjectRecord`) and for
every `ClientMessage` / `ServerMessage` / `RuntimeEvent` variant. The
SDK has no `rename_session` / `rename_project` message and no
`SessionRenamed` / `ProjectRenamed` event — renames are purely
app-side operations that never touch the runtime.

If you're tempted to add a `rename_X` message "because the app needs
it to persist across restarts," stop: that's the app store's job, not
the SDK's.

## When a new column looks runtime-essential

Verify by tracing: grep `runtime-core` / `orchestration` /
`daemon-core` / provider crates for the field. If nothing reads it,
the field is display-only regardless of how it feels. If it's read
once in a provider adapter but only to pass to a display message,
that's also display-only — the provider can route the value through
the event stream without persisting it.
