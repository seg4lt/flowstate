# zenui-provider-opencode

Flowstate adapter for [opencode](https://github.com/sst/opencode). Drives
opencode via its headless HTTP server (`opencode serve`) with a Server-Sent
Events stream for turn output.

**Layout**

- `src/lib.rs` — `OpenCodeAdapter` implementing `ProviderAdapter`.
- `src/server.rs` — one shared `opencode serve` subprocess per daemon.
- `src/http.rs` — REST client (`reqwest`) + permission/variant/slug helpers.
- `src/events.rs` — SSE reader and per-session event routing.

**Further reading**

- **[`PROTOCOL.md`](./PROTOCOL.md)** — the full wire-protocol reference:
  every endpoint we hit, every SSE event shape we parse, every gotcha we've
  discovered while reverse-engineering opencode's undocumented server API.
  Update it alongside the adapter whenever opencode changes.

**Running the tests**

```bash
# Fast in-crate tests (canned JSON fixtures, no network). ~60 ms.
cargo test -p zenui-provider-opencode

# Gated end-to-end smoke against a real `opencode serve`. Requires
# `opencode` on PATH and network access. ~10 s.
cargo test -p zenui-provider-opencode --test live_opencode \
    -- --ignored --nocapture
```
