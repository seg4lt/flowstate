# provider-github-copilot-cli

`ProviderAdapter` for the `gh copilot` CLI, using its JSON-RPC over
stdio protocol. No TypeScript bridge — talks directly to the user's
installed `gh` CLI with the copilot extension.

## How it works

- `GitHubCopilotCliAdapter` spawns `gh copilot` with the correct
  arguments and communicates via line-delimited JSON-RPC.
- Request / response pairs are matched by the JSON-RPC `id` field;
  streaming notifications are translated into `ProviderTurnEvent`s on
  the `TurnEventSink`.

## When to use

Lower-overhead than
[`../provider-github-copilot/`](../provider-github-copilot/README.md) —
relies on the user's existing `gh` CLI authentication and
configuration, and has no bundled Node runtime.

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
