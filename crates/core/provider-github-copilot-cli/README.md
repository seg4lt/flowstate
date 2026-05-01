# provider-github-copilot-cli

`ProviderAdapter` for the standalone `copilot` CLI (the npm package
`@github/copilot`), using its JSON-RPC over stdio protocol. No
TypeScript bridge — talks directly to the user's locally installed
`copilot` binary.

## How it works

- `GitHubCopilotCliAdapter` spawns `copilot` with the correct
  arguments and communicates via line-delimited JSON-RPC.
- Request / response pairs are matched by the JSON-RPC `id` field;
  streaming notifications are translated into `ProviderTurnEvent`s on
  the `TurnEventSink`.

## When to use

Lower-overhead than
[`../provider-github-copilot/`](../provider-github-copilot/README.md) —
relies on the user's existing `copilot` CLI install and
authentication, and has no bundled Node runtime.

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
