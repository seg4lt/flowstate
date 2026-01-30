# provider-claude-cli

`ProviderAdapter` for Claude via the `claude` CLI binary, spawned as a
child process and driven over its stream-json protocol on stdin / stdout.

## How it works

- No TypeScript bridge — this adapter talks directly to whatever
  `claude` binary is on the user's PATH.
- `ClaudeCliAdapter` invokes `claude` with the appropriate flags, sends
  prompt input on stdin, and parses the `stream-json` output into
  `ProviderTurnEvent`s.
- Permission prompts, tool calls, and subagent events are all mapped
  from the CLI's structured output to the runtime's event model.

## When to use

Pick this when you want to reuse the user's existing `claude` CLI
authentication and configuration instead of bridging the SDK directly.
Also useful when the Agent SDK isn't installable in the deployment
environment, or when you want the simplest possible adapter surface.

For richer event granularity, prefer
[`../provider-claude-sdk/`](../provider-claude-sdk/README.md).

## Dependencies

- `provider-api` — trait and event types.
- `tokio` — async process management.
