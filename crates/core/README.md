# core — shared domain

Transport-agnostic logic that defines what the agent SDK does,
independent of how it's wired up. Any frontend, transport adapter, or
test harness depends on these; none of these depend on any frontend,
transport, or daemon crate.

## What lives here

- **[`runtime-core/`](./runtime-core/README.md)** — `RuntimeCore` struct
  and the central event bus. Owns a `broadcast::Sender<RuntimeEvent>`,
  dispatches `ClientMessage`s, spawns provider-adapter tasks, drains
  streaming turn events, and persists results. The brain.
- **[`provider-api/`](./provider-api/README.md)** — The shared protocol.
  `ClientMessage`, `ServerMessage`, `RuntimeEvent`, the `ProviderAdapter`
  trait, and every session/turn/record type that crosses a boundary.
- **[`orchestration/`](./orchestration/README.md)** — Pure session and
  turn state machine. No I/O.
- **[`persistence/`](./persistence/README.md)** — SQLite storage via
  bundled `rusqlite`, wrapped in an async-safe `PersistenceService`.
- **[`provider-claude-sdk/`](./provider-claude-sdk/README.md)** — Claude
  via the Agent SDK (TypeScript bridge).
- **[`provider-codex/`](./provider-codex/README.md)** — OpenAI Codex CLI.
- **[`provider-github-copilot/`](./provider-github-copilot/README.md)** —
  GitHub Copilot via the SDK (TypeScript bridge).

## Why providers live in `core` and not `middleman`

Providers implement `ProviderAdapter` and are the concrete capability
that makes the SDK useful. They aren't transport glue — transport glue
moves bytes; providers produce the bytes in the first place. Without
them, `RuntimeCore` has nothing to dispatch to.
