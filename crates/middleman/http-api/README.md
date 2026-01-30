# http-api

The HTTP + WebSocket transport that fronts `RuntimeCore`. Built on
`axum` + `tower-http`. Binds loopback-only and serves both the React
frontend (static files) and the runtime API surface.

## Routes

| Method | Path | Purpose |
| --- | --- | --- |
| `GET`  | `/` and fallback | Static React app from `frontend/dist`. |
| `GET`  | `/api/health` | Liveness probe. |
| `GET`  | `/api/bootstrap` | Full `BootstrapPayload` — snapshot + providers + models + `ws_url`. |
| `GET`  | `/api/snapshot` | Just the session + project snapshot. |
| `GET`  | `/api/status` | `DaemonStatus` (counters + uptime + version). Returns 501 if no lifecycle observer is attached. |
| `POST` | `/api/shutdown` | Request graceful shutdown. 204 on success, 403 for non-loopback peers, 501 without a lifecycle. |
| `GET`  | `/ws` | WebSocket upgrade. Sends `Welcome { bootstrap }` then streams `RuntimeEvent`s. Accepts `ClientMessage` frames from the client. |

## Key types

- **`LocalServer`** — Returned by `spawn_local_server`. Drop it to
  shut the server down gracefully via its embedded oneshot.
- **`ConnectionObserver`** (trait) — The hook daemon-core implements.
  Methods: `on_client_connected`, `on_client_disconnected`,
  `on_shutdown_requested`, and an optional `status()` returning
  `DaemonStatus`. Non-daemon callers pass `None` and all hooks become
  no-ops.
- **`DaemonStatus`** — `{ connected_clients, in_flight_turns,
  uptime_seconds, daemon_version, started_at }`. Serialized by
  `/api/status`.

## `spawn_local_server`

```rust
pub fn spawn_local_server(
    runtime_handle: &tokio::runtime::Runtime,
    runtime: Arc<RuntimeCore>,
    frontend_dist: PathBuf,
    bind_addr: SocketAddr,
    lifecycle: Option<Arc<dyn ConnectionObserver>>,
) -> Result<LocalServer>
```

Binds synchronously via `std::net::TcpListener::bind`, hands the
listener to tokio, mounts the router, and returns.

## WebSocket loop

`handle_socket` runs three concurrent halves sharing a single outbound
mpsc:

1. **Writer** — drains the mpsc and writes each `ServerMessage` to the
   socket.
2. **Subscriber** — consumes the broadcast `RuntimeEvent` stream and
   forwards each event as `ServerMessage::Event`. On
   `RecvError::Lagged`, sends a fresh `ServerMessage::Snapshot` so the
   client re-reconciles from authoritative state.
3. **Receiver** — reads inbound frames, parses them as `ClientMessage`,
   and spawns a per-message task that dispatches into `RuntimeCore`
   and writes the result back through the shared mpsc.

The client-count `DisconnectGuard` RAII struct ensures
`on_client_disconnected` fires on every exit path, including panic
unwinds from any of the three halves.

## Invariant

`subscribe()` MUST be called before `bootstrap()` in `handle_socket`.
Any `RuntimeEvent` published between the bootstrap's database read and
the subscription call would otherwise land in a gap the client never
sees on reconnect. The invariant is protected by an explicit comment
at the call site in `handle_socket`.

## Dependencies

- `axum` (with `ws` feature), `tower-http`, `tokio`, `futures`.
- `runtime-core` — for `RuntimeCore` and subscription semantics.
- `provider-api` — for the message types carried on the wire.
