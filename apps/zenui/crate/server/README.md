# zenui-server — the daemon binary

The long-running background process that owns `RuntimeCore`, every
provider adapter, the SQLite database, and a caller-composed set of
transports. The desktop shell auto-spawns this process on launch and
attaches to it; closing the shell window leaves the daemon running so
in-flight provider turns complete.

## Subcommands

| Command | Purpose |
| --- | --- |
| `zenui-server start --foreground` | Run the daemon in this process. Logs to stderr. Blocks until shutdown. Useful for debugging. |
| `zenui-server start` | Detached mode. Fork-execs a child with `--foreground`, redirects stdio to `$TMPDIR/zenui/logs/zenui-server.log`, polls the ready file, returns when the child is live. |
| `zenui-server stop` | Read the ready file, find the HTTP transport entry, POST `/api/shutdown` to it, wait for the ready file to be deleted (up to 10s). |
| `zenui-server status` | Pretty-print the ready file contents (including every transport entry) plus a live `/api/status` probe from the first HTTP transport. |

All subcommands accept `--project-root PATH` (defaults to cwd). The
ready file is keyed by a hash of the canonical project root, so
running `zenui-server` in two different directories gives you two
independent daemons with separate SQLite databases.

`start` also accepts:

- `--bind 127.0.0.1:0` — override the HTTP bind address (defaults to
  a random loopback port).
- `--frontend-dist PATH` — override where `HttpTransport` reads the
  React bundle from.
- `--idle-timeout-secs 60` — seconds the `DaemonLifecycle::idle_watchdog`
  waits with zero connected clients and zero in-flight turns before
  firing.

## Transport composition

This crate explicitly composes the transport list in `main.rs`:

```rust
let transports: Vec<Box<dyn zenui_daemon_core::Transport>> =
    vec![Box::new(HttpTransport::new(bind_addr, frontend_dist))];

run_blocking(config, transports)?;
```

Adding a second transport is a one-line change in this file, not a
`daemon-core` patch. A future revision that also speaks a Unix socket
would look like:

```rust
let transports: Vec<Box<dyn Transport>> = vec![
    Box::new(HttpTransport::new(bind_addr, frontend_dist)),
    Box::new(UnixSocketTransport::new(socket_path)),
];
```

The daemon hosts both transports simultaneously, every client sees
the same `RuntimeCore`, and `DaemonLifecycle::connected_clients`
counts both kinds without caring about the wire format.

## Detached spawn (Unix)

`start` (without `--foreground`) fork-execs a child via
`std::process::Command` with a `pre_exec` closure that calls
`libc::setsid()`. The child runs `zenui-server start --foreground
...`, becomes its own session leader, and survives the parent shell
closing its TTY. Stdio is redirected to the log file before the
re-exec. Windows support (`CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`)
is a known Phase 4 follow-up.

## Frontend build

`build.rs` in this crate runs `bun install` + `bun run build` against
[`../../frontend/`](../../frontend/) (two levels up — the frontend
lives inside the same app). After the build completes, build.rs sets
`cargo:rustc-env=ZENUI_FRONTEND_DIST=<absolute dist path>`, which the
binary picks up at compile time via `env!("ZENUI_FRONTEND_DIST")` and
uses as the default `HttpTransport` frontend dir. The `--frontend-dist`
flag at runtime still overrides this.

## Dependencies

- `zenui-daemon-core` — for `DaemonConfig`, `run_blocking`, and the
  `Transport` trait.
- `zenui-transport-http` — for `HttpTransport`, the only transport
  shipped today.
- `clap` — subcommand parsing.
- `ureq` — blocking HTTP client for `stop` and the `status` probe.
- `libc` (unix only) — for `setsid` during detachment.
- `serde_json`, `anyhow`, `tokio`, `tracing`, `tracing-subscriber`.

## Binary target

```toml
[[bin]]
name = "zenui-server"
path = "src/main.rs"
```

## Related

- [`../../main-application/`](../../main-application/README.md) — the
  `zenui` desktop shell that attaches to this daemon via
  `daemon-client`.
- [`../../../../crates/middleman/daemon-core/`](../../../../crates/middleman/daemon-core/README.md)
  — where the daemon lifecycle and transport trait live.
- [`../../../../crates/middleman/transport-http/`](../../../../crates/middleman/transport-http/README.md)
  — the HTTP transport implementation.
