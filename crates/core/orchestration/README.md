# orchestration

Pure session and turn state machine. Operates on `SessionDetail`
in-memory and returns the resulting `TurnRecord` mutations to the
caller. No I/O, no persistence calls, no provider spawning — just
state transitions.

Separating this from `runtime-core` makes the state machine trivial to
unit-test without mocking a runtime, a provider, or SQLite.

## `OrchestrationService`

Stateless struct — all operations take `&self` and mutate the session
passed in by `&mut`. One instance is shared across the runtime.

| Method | Purpose |
| --- | --- |
| `new()` | Construct the (zero-state) service. |
| `create_session(provider, title, model, project_id)` | Mint a new `SessionDetail` with a fresh UUID and `Ready` status. |
| `start_turn(session, input, permission_mode, reasoning_effort)` | Append a `Running` turn to `session.turns`, flip `session.summary.status` to `Running`, return the new `TurnRecord`. |
| `finish_turn(session, turn_id, output, status)` | Mark the turn terminal. Flips the session back to `Ready` (success) or `Interrupted`, populates `last_turn_preview`. |
| `interrupt_session(session, message)` | Mark the most recent running turn as `Interrupted` with `message`, flip the session status. Used by post-crash startup reconciliation — the interactive stop flow goes through `finish_turn(Interrupted)` inside `runtime-core::send_turn` instead so it can preserve streamed blocks. |

## Dependencies

- `provider-api` — for every session / turn type.
- `chrono` — for timestamps.
- `uuid` — for session and turn IDs.
