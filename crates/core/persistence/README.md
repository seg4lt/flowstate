# persistence

SQLite-backed storage layer via bundled `rusqlite`. Every read and
write flows through `PersistenceService`, which wraps a single
`Mutex<Connection>` for synchronous single-writer semantics.

## `PersistenceService`

| Method | Purpose |
| --- | --- |
| `new(path)` | Open (or create) the database file, run migrations, return the service. |
| `in_memory()` | SQLite `:memory:` database for tests. Used by `runtime-core`'s unit tests. |
| `upsert_session(SessionDetail)` | Insert or replace by `session_id`. Serializes nested `turns` to JSON columns. |
| `get_session(id)` | Read one session. |
| `list_sessions()` | Read every session, newest first. |
| `delete_session(id)` | Returns `bool` indicating whether a row was actually removed. |
| `create_project(path)` / `delete_project(id)` / `list_projects()` | Project CRUD. `path` is optional; display labels (name, sort order) live in the consuming app's own store — see `CLAUDE.md` in this directory. Delete returns `(project_id, reassigned_session_ids)`. |
| `assign_session_to_project(session_id, project_id)` | Move a session between projects or out of all projects. |
| `get_cached_models(provider)` / `set_cached_models(provider, models)` | 24-hour provider model list cache, consulted by `RuntimeCore::bootstrap`. |

## Schema

Managed in-crate via migrations applied during `new()`. Tables include
`sessions`, `turns`, `projects`, and `provider_models`. Complex nested
structures from `provider-api` (turn records with tool calls, file
changes, subagents, plans) serialize to JSON columns rather than
normalizing at the SQL level.

## Concurrency

The `Mutex<Connection>` serializes all writes. This is fine at current
load but will bottleneck with many concurrent WebSocket clients. A
future upgrade path is `deadpool-sqlite` + WAL mode, currently out of
scope.

## Dependencies

- `provider-api` — for every persisted type.
- `rusqlite` with `bundled` feature — ships its own SQLite build.
- `tokio` — async `Mutex`.
- `chrono`, `serde_json` — timestamps and JSON column serde.
