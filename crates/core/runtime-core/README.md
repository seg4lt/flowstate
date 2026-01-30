# runtime-core

Central runtime for ZenUI. Owns session state, dispatches client
messages to provider adapters, drains streaming turn events, and
broadcasts `RuntimeEvent`s to attached subscribers.

## Key types

- **`RuntimeCore`** — the main struct. Usually shared as
  `Arc<RuntimeCore>` across the http-api layer and any in-process
  consumer.
- **`TurnLifecycleObserver`** — optional hook the daemon uses to count
  in-flight turns. `runtime-core` knows nothing about the daemon crate;
  it just calls `on_turn_start(session_id)` / `on_turn_end(session_id)`
  through this trait if one was passed to `RuntimeCore::new`. Default
  path (no observer) is a no-op.
- **`TurnCounterGuard`** — internal RAII wrapper that guarantees
  `on_turn_end` fires on every exit path (normal return, `?` early
  return, panic unwind). Non-negotiable: a panic inside an adapter's
  `execute_turn` cannot leak the daemon's `in_flight_turns` counter.

## Key methods

| Method | Purpose |
| --- | --- |
| `handle_client_message(ClientMessage)` | Inbound dispatch. Matches the message and either responds directly, mutates session state, or spawns a provider turn. |
| `subscribe()` | Returns a `broadcast::Receiver<RuntimeEvent>`. Multiple subscribers allowed; lagged subscribers recover via a fresh snapshot in the http-api layer. |
| `publish(RuntimeEvent)` | Sync broadcast. Drops events cleanly when there are zero subscribers. |
| `bootstrap(ws_url)` | Returns a `BootstrapPayload`: snapshot + provider health + cached models. Kicks off a background model refresh for stale caches. |
| `snapshot()` | Returns just the persisted session + project state. |
| `reconcile_startup()` | Walks persisted sessions on boot, flips any session stuck at `SessionStatus::Running` to `Interrupted`. Must be called once at startup, before serving clients. Fixes the "daemon crashed mid-turn" latent bug. |
| `shutdown_all_turns(grace)` | Graceful shutdown sweep. Calls `interrupt_turn` on every active session and loops-and-re-snapshots until `active_sinks` drains or `grace` elapses. |

## Invariants

- Turns run as detached `tokio::spawn` tasks; the drain loop owns the
  event mpsc. The WebSocket handler can drop at any time without
  aborting a turn — this is the whole premise of session survival.
- `publish()` is sync and drops events on no subscribers, so turn
  progress never blocks on client delivery.
- All provider turn events flow through the runtime's internal drain
  loop, not through any external channel. The drain loop is what
  translates `ProviderTurnEvent` into `RuntimeEvent` and persists
  per-turn record updates.

## Dependencies

- `provider-api` — for every shared type
- `orchestration` — for session / turn state transitions
- `persistence` — for SQLite reads and writes
