# apps/zenui/crate — app-specific sub-crates

Rust crates specific to the ZenUI app. Each crate here is a sibling of
[`../main-application/`](../main-application/README.md), broken out for
clarity rather than shared use — nothing outside `apps/zenui/` depends
on anything in this directory.

Shared crates live under `crates/core/` and `crates/middleman/` at the
workspace root. This directory is strictly for code that's specific
to zenui and wouldn't be reused by a second app.

## Crates

- **[`server/`](./server/README.md)** — the `zenui-server` daemon
  binary. Parses clap subcommands (`start` / `stop` / `status`) and
  delegates to `zenui-daemon-core` for the actual runtime wiring.
