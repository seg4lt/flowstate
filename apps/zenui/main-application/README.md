# zenui — desktop shell (main application)

The binary a user launches when they type `zenui`. A native
undecorated tao + wry window that attaches to a local `zenui-server`
daemon and loads the React frontend served by that daemon.

## What it does

1. Resolves the project root (canonical current working directory).
2. Calls `daemon_client::connect_or_spawn` to locate a running
   `zenui-server` for this project, or auto-spawn one under an
   advisory lock if none exists.
3. Opens a native undecorated window (1400 × 900 default, transparent
   titlebar on macOS).
4. Embeds a wry webview pointed at `handle.http_base`.
5. Installs a wry IPC handler for window-chrome commands
   (`minimize` / `maximize` / `close` / `drag`) — these stay in-process
   and never touch the daemon.
6. On macOS, installs a `muda` menu bar so `⌘C` / `⌘V` / `⌘X` / `⌘A`
   work inside the webview.
7. Runs the tao event loop until the window closes.

Closing the window exits the shell process; the daemon keeps running
until its idle watchdog fires (default 60s without any client
connected and no in-flight turns).

## Dependencies

- `zenui-daemon-client` — all the daemon discovery, auto-spawn, and
  ready-file logic.
- `tao`, `wry`, `muda` — window, webview, macOS menu bar.
- `anyhow`, `serde`, `serde_json`.

**Deliberately no dependency on** `runtime-core`, any provider,
`persistence`, `daemon-core`, or `http-api`. The shell stays lean so
rebuilding it is fast, and so there's no code path by which it could
accidentally acquire the runtime in-process.

## Binary target

```toml
[[bin]]
name = "zenui"
path = "src/main.rs"
```

The binary is named `zenui` (not `zenui-tao-web-shell`) so the user
command-line surface is simple.

## Related

- [`../crate/server/`](../crate/server/README.md) — the `zenui-server`
  daemon binary this shell attaches to.
- [`../README.md`](../README.md) — ZenUI app overview.
- [`../../../README.md`](../../../README.md) — full architecture and
  build instructions.
