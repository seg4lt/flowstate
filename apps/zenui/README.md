# apps/zenui — the ZenUI desktop app

A native desktop AI coding agent shell. Runs as two cooperating binaries:

- **[`main-application/`](./main-application/README.md)** — the `zenui`
  binary the user launches. A tao + wry native window that auto-attaches
  to (or auto-spawns) a local `zenui-server` daemon and loads the React
  frontend served by that daemon.
- **[`crate/server/`](./crate/server/README.md)** — the `zenui-server`
  binary. The daemon itself: owns `RuntimeCore`, every provider adapter,
  the SQLite database, and the HTTP + WS transport. Runs on its own
  tokio runtime so provider turns continue executing when the shell
  window is closed.

## Why two binaries

**Session survival.** If the desktop shell owned the runtime, closing
the window would kill any in-flight Claude / Codex / Copilot turn. With
the runtime in a separate daemon process, the user can close the window
mid-refactor, come back later, and find the completed result waiting.

## Auto-spawn flow

```
  user launches `zenui`
       │
       ▼
  daemon_client::connect_or_spawn
       │
       ├─► ready file healthy  ──► attach to existing daemon
       │
       └─► no ready file / stale
                │
                ├─► acquire spawn lock (fs4 advisory)
                │
                └─► fork-exec `zenui-server start`
                         │
                         ▼
                    detached daemon polls its own
                    ready file, returns when live
                         │
                         ▼
                    shell connects, opens webview
```

See the top-level [`../../README.md`](../../README.md) for the full
architecture diagram and build instructions.
