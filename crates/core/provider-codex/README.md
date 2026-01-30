# provider-codex

`ProviderAdapter` for OpenAI Codex via the Codex CLI tool, spawned as
a subprocess and driven over its native stdio protocol.

## How it works

- `CodexAdapter` spawns `codex` with the correct flags, writes the
  turn input, and parses Codex's streaming output into
  `ProviderTurnEvent`s.
- Sessions persist across turns by resuming Codex's *rollout* thread
  ID. The adapter stores that ID in `ProviderSessionState` between
  turns so a subsequent `send_turn` picks up where the previous one
  left off.
- Treats "no rollout found" as a recoverable thread-resume error —
  the adapter transparently starts a fresh conversation when the
  prior rollout has been pruned.

## When to use

For any interaction with the OpenAI Codex CLI. No bridge subprocess or
TypeScript sidecar needed — just the `codex` binary on PATH.

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
