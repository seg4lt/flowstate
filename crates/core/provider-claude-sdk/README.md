# provider-claude-sdk

`ProviderAdapter` for Anthropic Claude via the official Agent SDK,
bridged through a TypeScript sidecar that this crate builds and bundles.

## How it works

- The `bridge/` subdirectory holds a small TypeScript program that
  imports `@anthropic-ai/claude-agent-sdk` and exposes a stdio
  JSON protocol compatible with our `ProviderTurnEvent` model.
- `build.rs` downloads an isolated Node.js runtime into the crate's
  `OUT_DIR`, runs `bun` to compile the TypeScript, and copies the
  compiled bridge and its `node_modules` into `OUT_DIR`. The result
  is a self-contained provider that does not rely on the user's
  system Node installation.
- At runtime, `ClaudeSdkAdapter` spawns the bridge process via
  `tokio::process::Command`, writes JSON requests to stdin, and
  decodes JSON events from stdout, forwarding them as
  `ProviderTurnEvent`s through the `TurnEventSink`.

## When to use

Prefer the SDK adapter for richer streaming semantics — granular
tool-call events, plan proposals, subagent lifecycle — and for
programmatic control over permission decisions.

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
- Provider-specific: the bundled Node runtime and compiled bridge
  code produced by `build.rs`.
