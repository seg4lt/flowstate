# transport-http

HTTP + WebSocket transport for the ZenUI daemon. Implements the
`Transport` trait from `zenui-daemon-core` via a two-stage
`HttpTransport` → `HttpBound` → `HttpHandle` lifecycle. Built on
`axum` + `tower-http`.

## Where it plugs in

```
apps/zenui/crate/server/src/main.rs
 └─ vec![Box::new(HttpTransport::new(bind_addr, frontend_dist))]
     │
     ▼
zenui_daemon_core::run_blocking(config, transports)
     │
     ├─► HttpTransport::bind()    (host thread, claims TCP socket)
     ├─► HttpBound::serve()        (tokio ctx, spawns accept loop)
     └─► HttpHandle::shutdown()    (async, drains connections)
```

`daemon-core` does NOT depend on this crate. The dependency direction
is `transport-http → daemon-core` (for the trait) and
`transport-http → runtime-core` (for `RuntimeCore` +
`ConnectionObserver`). Apps that don't need HTTP/WS simply don't link
`zenui-transport-http`.

## Routes

| Method | Path | Purpose |
| --- | --- | --- |
| `GET`  | `/` and fallback | Static React app from `frontend_dist`. |
| `GET`  | `/api/health` | Liveness probe. |
| `GET`  | `/api/bootstrap` | Full `BootstrapPayload` — snapshot + providers + models + `ws_url`. |
| `GET`  | `/api/snapshot` | Just the session + project snapshot. |
| `GET`  | `/api/status` | `DaemonStatus` from the observer. 501 when `NoopObserver` is wired. |
| `POST` | `/api/shutdown` | Request graceful shutdown. 204 on success, 403 for non-loopback peers. |
| `GET`  | `/ws` | WebSocket upgrade. Sends `Welcome { bootstrap }` then streams `RuntimeEvent`s. Accepts `ClientMessage` frames from the client. |

## `HttpTransport::new`

```rust
pub fn new(bind_addr: SocketAddr, frontend_dist: PathBuf) -> Self
```

Holds only configuration. `bind()` synchronously claims the TCP socket
on the host thread, verifies `frontend_dist/index.html` exists, and
returns `Box<dyn Bound>`. `serve()` — called inside the tokio runtime —
wraps the `StdTcpListener` in `TcpListener::from_std`, spawns the axum
accept loop, and returns a `Box<dyn TransportHandle>`.

## `ConnectionObserver` is non-optional

`HttpBound::serve` takes `observer: Arc<dyn ConnectionObserver>` by
value. Callers who don't want observation pass `Arc::new(NoopObserver)`
from `zenui-runtime-core`. All the `Option::map` chains in the old
handlers are gone.

## WebSocket loop

`handle_socket` runs three concurrent halves sharing a single outbound
mpsc:

1. **Writer** — drains the mpsc and writes each `ServerMessage` to the
   WebSocket.
2. **Subscriber** — consumes the broadcast `RuntimeEvent` stream and
   forwards each event as `ServerMessage::Event`. On
   `RecvError::Lagged`, sends a fresh `ServerMessage::Snapshot` so the
   client re-reconciles from authoritative state.
3. **Receiver** — reads inbound frames, parses them as `ClientMessage`,
   and spawns a per-message task that dispatches into `RuntimeCore` and
   writes the result back through the shared mpsc.

A `DisconnectGuard` RAII struct ensures `observer.on_client_disconnected()`
fires on every exit path, including panic unwinds from any of the
three halves.

## Invariant

`subscribe()` MUST be called before `bootstrap()` in `handle_socket`.
Any `RuntimeEvent` published between the bootstrap's database read and
the subscription call would otherwise land in a gap the client never
sees on reconnect. The invariant is protected by an explicit comment
at the call site.

## Dependencies

- `zenui-daemon-core` — for the `Transport` / `Bound` / `TransportHandle`
  trait definitions and `TransportAddressInfo`.
- `zenui-runtime-core` — for `RuntimeCore`, `ConnectionObserver`.
- `zenui-provider-api` — for the wire types.
- `axum` (with `ws`), `tower-http`, `tokio`, `futures`, `async-trait`.
