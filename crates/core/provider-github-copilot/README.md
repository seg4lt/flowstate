# provider-github-copilot

`ProviderAdapter` for GitHub Copilot Chat via a TypeScript bridge to
the underlying copilot SDK.

## How it works

Same pattern as
[`../provider-claude-sdk/`](../provider-claude-sdk/README.md):

- The `bridge/` subdirectory holds a TypeScript sidecar that imports
  the copilot SDK and exposes a stdio JSON protocol.
- `build.rs` downloads an isolated Node runtime, runs `bun` to compile
  the TypeScript bridge, and bundles the result into the crate's
  `OUT_DIR`.
- `GitHubCopilotAdapter` spawns the bridge at runtime and translates
  its output into `ProviderTurnEvent`s.

## When to use

Use this for GitHub Copilot integration with deep tool-call support
and programmatic permission handling.

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
- Provider-specific: the bundled Node runtime and compiled bridge code
  produced by `build.rs`.
