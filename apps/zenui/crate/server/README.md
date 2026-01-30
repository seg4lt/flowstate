# zenui-server — the daemon binary

The long-running background process that owns `RuntimeCore`, every
provider adapter, the SQLite database, and the HTTP + WS transport.
The desktop shell auto-spawns this process on launch and attaches to
it; closing the shell window leaves the daemon running so in-flight
provider turns complete.

## Subcommands

| Command | Purpose |
| --- | --- |
| `zenui-server start --foreground` | Run the daemon in this process. Logs to stderr. Blocks until shutdown. Useful for debugging. |
| `zenui-server start` | Detached mode. Fork-execs a child with `--foreground`, redirects stdio to `$TMPDIR/zenui/logs/zenui-server.log`, polls the ready file, returns when the child is live. |
| `zenui-server stop` | Read the ready file, POST `/api/shutdown`, wait for the ready file to be deleted (up to 10 s). |
| `zenui-server status` | Pretty-print the ready file contents plus a live `/api/status` probe (counters, uptime, version). |

All subcommands accept `--project-root PATH` (defaults to the current
working directory). The ready file is keyed by a hash of the canonical
project root, so running `zenui-server` in two different directories
gives you two independent daemons with separate SQLite databases.

`start` also accepts:

- `--bind 127.0.0.1:0` — override the bind address (defaults to a
  random loopback port).
- `--frontend-dist PATH` — override where the daemon reads the React
  frontend bundle from.
- `--idle-timeout-secs 60` — how long to wait with zero connected
  clients and zero in-flight turns before the idle watchdog fires.

## Detached spawn (Unix)

Uses `pre_exec(|| libc::setsid())` so the child becomes its own
session leader and survives the parent shell closing its TTY. stdio
is redirected to the log file before the re-exec. Windows support
(DETACHED_PROCESS + CREATE_NEW_PROCESS_GROUP) is a known Phase 4
follow-up.

## Frontend build

`build.rs` in this crate runs `bun install` + `bun run build` against
`frontend/` (four levels up the workspace tree) to produce
`frontend/dist/`. The daemon serves that directory as static files
at `http://127.0.0.1:<port>/`.

## Dependencies

- `zenui-daemon-core` — every bit of actual runtime and lifecycle
  logic. This crate is just CLI plumbing over `run_blocking`.
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
  `zenui` desktop shell that attaches to this daemon.
- [`../../../../crates/middleman/daemon-core/`](../../../../crates/middleman/daemon-core/README.md)
  — where the real work happens.
- [`../../README.md`](../../README.md) — ZenUI app overview.
