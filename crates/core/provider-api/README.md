# provider-api

The shared protocol crate. Every other crate in ZenUI speaks these
types when a message crosses a boundary — into or out of the runtime,
over the wire to the frontend, across the daemon IPC transport, or
between a provider adapter and its external process.

## The wire protocol

- **`ClientMessage`** (serde-tagged enum) — inbound commands from a
  client. Variants include `StartSession`, `SendTurn`, `InterruptTurn`,
  `AnswerPermission`, `AnswerQuestion`, `AcceptPlan`, `RejectPlan`,
  `LoadSnapshot`, `Ping`, plus project CRUD commands.
- **`ServerMessage`** (serde-tagged enum) — outbound responses to a
  `ClientMessage`: `Welcome { bootstrap }`, `Snapshot`, `SessionCreated`,
  `Ack`, `Error`, `Event { RuntimeEvent }`, `Pong`.
- **`RuntimeEvent`** (serde-tagged enum) — broadcast events fired by
  the runtime to every subscribed client: `RuntimeReady`,
  `DaemonShuttingDown`, `SessionStarted`, `TurnStarted`, `ContentDelta`,
  `ReasoningDelta`, `ToolCallStarted`, `ToolCallCompleted`,
  `PermissionRequested`, `UserQuestionAsked`, `FileChanged`,
  `SubagentStarted`, `PlanProposed`, `TurnCompleted`,
  `SessionInterrupted`, `SessionDeleted`, plus project / model events.

## The adapter contract

- **`ProviderAdapter`** (async trait) — every concrete provider crate
  implements this. Methods:
  - `kind()` — which `ProviderKind` this adapter handles.
  - `health()` — probe for availability / authentication.
  - `start_session()` — create any per-session provider state.
  - `execute_turn()` — run one conversation turn, streaming events
    through a `TurnEventSink`.
  - `interrupt_turn()` — cancel an in-flight turn.
  - `end_session()` — tear down per-session resources.
  - `fetch_models()` — list available model identifiers.
- **`TurnEventSink`** — streaming channel an adapter uses to push
  `ProviderTurnEvent`s into the runtime's drain loop during
  `execute_turn`. Also carries permission / question callback plumbing.
- **`ProviderTurnEvent`** — granular events an adapter emits during a
  turn: text deltas, reasoning deltas, tool call lifecycle, subagent
  lifecycle, permission requests, user questions, plan proposals.

## Session state types

`SessionDetail`, `SessionSummary`, `TurnRecord`, `ToolCall`,
`FileChangeRecord`, `SubagentRecord`, `PlanRecord`,
`ProviderSessionState`, plus enum types (`SessionStatus`, `TurnStatus`,
`ProviderKind`, `PermissionMode`, `PermissionDecision`, `PlanStatus`,
`ReasoningEffort`, `ProviderStatusLevel`) — everything the runtime
persists and the frontend reads.

## Dependencies

Only `serde`, `serde_json`, `chrono`, and `async-trait`. No internal
deps. This is the foundation — every other crate depends on it.
