import * as React from "react";
import { ChevronDown, ChevronUp, Timer } from "lucide-react";
import type {
  AttachmentRef,
  ContentBlock,
  ProviderKind,
  SubagentRecord,
  ToolCall,
  TurnStatus,
} from "@/lib/types";
import { ToolCallCard } from "../tool-call-card";
import { ToolOutputContent } from "../tool-renderers";
import { UserMessage } from "./user-message";
import { AgentMessage } from "./agent-message";
import { CompactBlock } from "./compact-block";
import { MemoryRecallBlock } from "./memory-recall-block";
import { RewindDivider } from "./rewind-divider";
import { useEditStandaloneSetting } from "@/hooks/use-edit-standalone-setting";

const GROUP_DEFAULT_VISIBLE = 5;

// A render-layer block that consolidates consecutive `tool_call`
// blocks issued by the same agent into a single group. A change in
// `parentCallId` (main → sub-agent, sub-agent → main, or between two
// different sub-agents) starts a new group, as does any intervening
// text/reasoning block.
type RenderBlock =
  | { kind: "text"; text: string; key: string }
  | { kind: "reasoning"; text: string; key: string }
  | {
      kind: "tool_call_group";
      callIds: string[];
      parentCallId: string | undefined;
      key: string;
      /** Optional auto-generated label (~30 chars, "git-commit-
       *  subject" style) the provider produced for this batch of
       *  tool calls. When present the group renders collapsed-by-
       *  default with the label as the clickable header — mobile-
       *  density gain on long tool-heavy turns. Set during a
       *  second pass after the walking grouping logic, by matching
       *  any of a `ContentBlock::ToolUseSummary` block's
       *  `callIds` against the group's. Null = no auto-summary,
       *  group renders as before with overflow-only collapse. */
      summary?: string;
      /** When true, every `ToolCallCard` inside the group renders
       *  pre-expanded. Set by the edit-standalone code path so
       *  broken-out Edit calls show their diff inline without an
       *  extra click. Off everywhere else — default collapsed cards
       *  remain the baseline. */
      cardsDefaultOpen?: boolean;
    }
  | {
      kind: "compact";
      trigger: "auto" | "manual";
      preTokens?: number;
      postTokens?: number;
      durationMs?: number;
      summary?: string;
      key: string;
    }
  | {
      kind: "memory_recall";
      mode: "select" | "synthesize";
      memories: import("@/lib/types").MemoryRecallItem[];
      key: string;
    };

function groupBlocks(
  blocks: ContentBlock[],
  callsById: Map<string, ToolCall>,
  options: { editStandalone?: boolean } = {},
): RenderBlock[] {
  const { editStandalone = false } = options;
  const result: RenderBlock[] = [];

  // Main-agent grouping: sequential, breaks on any non-tool block or
  // when a sub-agent tool call interrupts the streak. The reference is
  // mutated in place — pushing once into result and then appending to
  // .callIds keeps the box stable across streaming updates.
  let currentMainGroup:
    | {
        kind: "tool_call_group";
        callIds: string[];
        parentCallId: string | undefined;
        key: string;
      }
    | null = null;

  // Sub-agent boxes: one per parentCallId, deduped via this Map. A box
  // can be seeded two ways: (1) by the dispatcher Task tool call itself
  // when it appears in the stream (hoisted out of the main group so the
  // user sees the spawn and its activity in one place), or (2) by the
  // first child tool call if the dispatcher hasn't arrived yet. Later
  // child tool calls from the SAME sub-agent append to the existing
  // box's callIds array in place — the user sees one persistent box
  // per sub-agent collecting all of its activity at the dispatcher's
  // original stream position. Parallel sub-agents land in separate
  // boxes because each has a different parentCallId.
  const subagentBoxes = new Map<
    string,
    {
      kind: "tool_call_group";
      callIds: string[];
      parentCallId: string | undefined;
      key: string;
    }
  >();

  // Callsids that spawned at least one sub-agent tool call. These
  // dispatcher tool calls are hoisted out of the main-agent group and
  // rendered as sub-agent boxes instead — everything the sub-agent
  // produced (tool calls + final output text) lives in one place.
  const dispatcherIds = new Set<string>();
  for (const tc of callsById.values()) {
    if (tc.parentCallId) dispatcherIds.add(tc.parentCallId);
  }

  blocks.forEach((block, idx) => {
    if (block.kind === "tool_call") {
      if (dispatcherIds.has(block.callId)) {
        // Main-agent tool call that spawned a sub-agent. Seed the
        // sub-agent box at THIS position and skip the main-group push.
        // The box's parentCallId matches this dispatcher's callId,
        // which is also the parentCallId every child tool call carries.
        currentMainGroup = null;
        if (!subagentBoxes.has(block.callId)) {
          const box = {
            kind: "tool_call_group" as const,
            callIds: [],
            parentCallId: block.callId,
            key: `tg-sub-${block.callId}`,
          };
          subagentBoxes.set(block.callId, box);
          result.push(box);
        }
        return;
      }

      const tc = callsById.get(block.callId);
      if (
        tc &&
        tc.parentCallId === undefined &&
        (tc.name === "TodoWrite" || tc.name === "ExitPlanMode")
      ) {
        // Main-agent todos and plans are surfaced exclusively through
        // the Agent Context side pane, so they get filtered out of
        // the inline flow here. currentMainGroup is intentionally not
        // reset — surrounding main-agent tool calls should stay in
        // one clean streak.
        return;
      }

      const parent = tc?.parentCallId;

      if (parent === undefined) {
        // Main agent — sequential grouping.
        //
        // Edit-standalone opt-in: every Edit / MultiEdit / Write call
        // breaks the current main-agent streak and lands in its own
        // single-call group. Subsequent main-agent tool calls start a
        // FRESH group on the other side — never merging through one
        // of these. Write piggybacks on the same toggle because a
        // brand-new file is just as much a "diff worth seeing inline"
        // as an Edit; users who want one broken out almost always
        // want the other. Sub-agent boxes are unaffected (their
        // identity is the parentCallId, not stream contiguity), so
        // these calls inside a Task's sub-agent still fold into that
        // sub-agent's box.
        const isStandaloneEdit =
          editStandalone &&
          (tc?.name === "Edit" ||
            tc?.name === "MultiEdit" ||
            tc?.name === "Write");
        if (isStandaloneEdit) {
          currentMainGroup = null;
          result.push({
            kind: "tool_call_group",
            callIds: [block.callId],
            parentCallId: undefined,
            key: `tg-edit-${block.callId}`,
            cardsDefaultOpen: true,
          });
          return;
        }
        if (currentMainGroup) {
          currentMainGroup.callIds.push(block.callId);
          return;
        }
        currentMainGroup = {
          kind: "tool_call_group",
          callIds: [block.callId],
          parentCallId: undefined,
          key: `tg-${block.callId}`,
        };
        result.push(currentMainGroup);
        return;
      }

      // Sub-agent child — find or create the persistent box for this
      // parent. A sub-agent block always breaks the current main-agent
      // streak, so the next main tool call starts fresh.
      currentMainGroup = null;
      const existing = subagentBoxes.get(parent);
      if (existing) {
        existing.callIds.push(block.callId);
        return;
      }
      const box = {
        kind: "tool_call_group" as const,
        callIds: [block.callId],
        parentCallId: parent,
        // Keyed by parentCallId so the expanded state stays stable as
        // more tool calls get appended over the life of the sub-agent.
        key: `tg-sub-${parent}`,
      };
      subagentBoxes.set(parent, box);
      result.push(box);
      return;
    }

    // Any non-tool block (text, reasoning, compact, memory_recall)
    // breaks the main-agent streak. Sub-agent boxes are unaffected
    // — they keep collecting across these interruptions because
    // their identity is the parentCallId, not stream contiguity.
    currentMainGroup = null;
    if (block.kind === "text") {
      result.push({ kind: "text", text: block.text, key: `text-${idx}` });
    } else if (block.kind === "reasoning") {
      result.push({
        kind: "reasoning",
        text: block.text,
        key: `reasoning-${idx}`,
      });
    } else if (block.kind === "compact") {
      result.push({
        kind: "compact",
        trigger: block.trigger,
        preTokens: block.preTokens,
        postTokens: block.postTokens,
        durationMs: block.durationMs,
        summary: block.summary,
        key: `compact-${idx}`,
      });
    } else if (block.kind === "memory_recall") {
      result.push({
        kind: "memory_recall",
        mode: block.mode,
        memories: block.memories,
        key: `memrecall-${idx}`,
      });
    }
    // tool_use_summary blocks are NOT emitted as their own visual
    // item — they get consumed in the second pass below and
    // attached to the matching tool_call_group as its summary
    // header. Falling through silently here is intentional.
  });

  // Second pass: walk the original block list once more and
  // attach each `tool_use_summary`'s text to the tool_call_group
  // whose callIds intersect. Most calls land in exactly one
  // group; if a summary's callIds span groups (rare — would mean
  // the provider summarized across a text/reasoning gap), we
  // attach to the first match to avoid duplicating the label.
  for (const block of blocks) {
    if (block.kind !== "tool_use_summary") continue;
    const targetIds = new Set(block.callIds);
    if (targetIds.size === 0 || !block.summary) continue;
    for (const item of result) {
      if (item.kind !== "tool_call_group") continue;
      if (item.summary) continue; // already labeled — first wins
      if (item.callIds.some((id) => targetIds.has(id))) {
        item.summary = block.summary;
        break;
      }
    }
  }

  return result;
}

function ToolCallGroup({
  callIds,
  parentCallId,
  callsById,
  subagentsByParent,
  summary,
  cardsDefaultOpen = false,
}: {
  callIds: string[];
  parentCallId: string | undefined;
  callsById: Map<string, ToolCall>;
  /** Optional record of every subagent in the turn, keyed by the
   *  dispatcher's call id (== `parentCallId`). Used to surface the
   *  per-subagent model in the box header. */
  subagentsByParent: Map<string, SubagentRecord>;
  /** Auto-generated ~30-char label for the batch (Claude SDK's
   *  `tool_use_summary`). When present, the group renders
   *  collapsed-by-default with this string as the clickable
   *  header — mobile-density gain on tool-heavy turns. */
  summary?: string;
  /** When true, every `ToolCallCard` inside this group renders
   *  pre-expanded. The edit-standalone code path sets this so
   *  broken-out Edits show their diff without an extra click. */
  cardsDefaultOpen?: boolean;
}) {
  // Always default to the overflow-collapsed state — show top 5
  // tool calls with a "Show N more" counter, click to expand. This
  // is the historical behavior. (When `summary` is set the whole
  // <details> additionally starts closed via `open={...}` below;
  // that's an orthogonal collapse layer for batch labels.)
  const [expanded, setExpanded] = React.useState(false);

  const calls = React.useMemo(() => {
    const out: ToolCall[] = [];
    for (const id of callIds) {
      const tc = callsById.get(id);
      if (tc) out.push(tc);
    }
    return out;
  }, [callIds, callsById]);

  // Sub-agent groups get a visible header so the user can see which
  // dispatch issued them and which agent type is running. The agent
  // type lives in the spawning Task tool's args (`subagent_type`),
  // and that Task tool call is in callsById keyed by parentCallId
  // because parentCallId is the call_id of the Task that spawned
  // this sub-agent. Falls back to the tool name and finally to
  // "Subagent" if neither is available (e.g. a sub-agent whose
  // parent Task call hasn't streamed in yet).
  const isSubagent = parentCallId !== undefined;
  const parentCall = isSubagent ? callsById.get(parentCallId) : undefined;
  const subagentType = isSubagent
    ? (parentCall?.args as { subagent_type?: string } | undefined)
        ?.subagent_type
    : undefined;
  const agentLabel = subagentType ?? parentCall?.name ?? "Subagent";

  // A sub-agent box may legitimately have zero children early in the
  // stream (dispatcher seen, first child not yet). Keep it visible if
  // we at least have the dispatcher's output/error to show.
  if (calls.length === 0 && !parentCall?.output && !parentCall?.error) {
    return null;
  }

  const overflow = calls.length - GROUP_DEFAULT_VISIBLE;
  const hasOverflow = overflow > 0;
  const visible =
    expanded || !hasOverflow ? calls : calls.slice(0, GROUP_DEFAULT_VISIBLE);

  const body = (
    <>
      <div className="divide-y divide-border/30">
        {visible.map((tc) => (
          <ToolCallCard
            key={tc.callId}
            toolCall={tc}
            defaultOpen={cardsDefaultOpen}
          />
        ))}
      </div>
      {hasOverflow && (
        <button
          type="button"
          onClick={() => setExpanded((e) => !e)}
          className="mt-1 inline-flex items-center gap-1 rounded-md px-2 py-0 text-[10px] leading-5 text-muted-foreground hover:bg-muted/50 hover:text-foreground"
        >
          {expanded ? (
            <>
              <ChevronUp className="h-3 w-3" />
              Show top {GROUP_DEFAULT_VISIBLE}
            </>
          ) : (
            <>
              <ChevronDown className="h-3 w-3" />
              Show {overflow} more
            </>
          )}
        </button>
      )}
    </>
  );

  if (isSubagent) {
    // The dispatcher Task's status reflects the sub-agent's own
    // lifecycle: pending while the sub-agent is running, completed when
    // it returns, failed if it errored. Surface it in the summary so
    // the user can scan activity without opening the box.
    const status = parentCall?.status;
    const statusText =
      status === "completed"
        ? "completed"
        : status === "failed"
          ? "failed"
          : status === "pending"
            ? "pending"
            : null;
    const statusClass =
      status === "completed"
        ? "text-green-600 dark:text-green-400"
        : status === "failed"
          ? "text-destructive"
          : "animate-pulse text-muted-foreground";

    // Subtext under the agent label: prefer the Task's description arg
    // (the natural-language one-liner like "Find tool call UI
    // rendering"), fall back to the agent type so the subtext is
    // always informative.
    const description = (
      parentCall?.args as { description?: string } | undefined
    )?.description;
    const subtext = description ?? agentLabel;

    // Pull the model this specific subagent ran on, when the record
    // exists and carries it. Populated lazily: the catalog value
    // shows up at spawn time if the Claude SDK exposed one, else
    // the first assistant message from the subagent overwrites it
    // with the observed pinned id.
    const subagentRecord = parentCallId
      ? subagentsByParent.get(parentCallId)
      : undefined;
    const subagentModel = subagentRecord?.model;

    return (
      <details
        open
        className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
      >
        <summary className="cursor-pointer select-none text-[10px] font-medium uppercase tracking-wide text-muted-foreground hover:text-foreground">
          ↳ {agentLabel}{" "}
          {subagentModel && (
            <span className="text-muted-foreground/60 normal-case tracking-normal">
              · <span className="font-mono">{subagentModel}</span>
            </span>
          )}
          <span className="text-muted-foreground/60">· {calls.length}</span>
          {statusText && (
            <span className={`ml-1 ${statusClass}`}>· {statusText}</span>
          )}
          <div className="mt-0.5 truncate text-[11px] font-normal normal-case tracking-normal text-muted-foreground/80">
            Subagent - {subtext}
          </div>
        </summary>
        <div className="mt-1.5">
          {body}
          {(parentCall?.output || parentCall?.error) && (
            <div className="mt-2 border-t border-border/30 pt-2">
              {parentCall?.output && (
                <ToolOutputContent output={parentCall.output} />
              )}
              {parentCall?.error && (
                <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap rounded bg-muted/60 p-2 text-[11px] text-destructive">
                  {parentCall.error}
                </pre>
              )}
            </div>
          )}
        </div>
      </details>
    );
  }

  // When the provider supplied an auto-summary, default the
  // group to collapsed so the label is the dominant visual — the
  // whole point of `tool_use_summary` is mobile-density on tool-
  // heavy turns. Without a summary, keep the historical
  // behavior (open by default).
  return (
    <details
      open={summary == null ? true : undefined}
      className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
    >
      <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
        {summary ? (
          <span className="flex items-center gap-1.5">
            <span className="font-medium text-foreground">{summary}</span>
            <span className="text-muted-foreground/60">
              · {calls.length} {calls.length === 1 ? "tool" : "tools"}
            </span>
          </span>
        ) : (
          <>
            Tools{" "}
            <span className="text-muted-foreground/60">· {calls.length}</span>
          </>
        )}
      </summary>
      <div className="mt-1.5">{body}</div>
    </details>
  );
}

// Normalized shape that covers both completed TurnRecords and the
// synthetic streaming row. `input` is null for streaming items because
// the store does not expose the pending user input — matches the
// previous behavior where the user message only appears after
// turn_completed fires.
export interface MessageItem {
  turnId: string;
  input: string | null;
  status: TurnStatus;
  // Canonical ordered content stream — text, reasoning, and tool-call
  // positions in the order the provider emitted them. Tool-call blocks
  // reference toolCalls[] by callId.
  blocks: ContentBlock[];
  toolCalls: ToolCall[] | null;
  streaming: boolean;
  /** References to images the user pasted on this turn. None on the
   * optimistic-echo row — they only appear once `turn_started` fires
   * and the daemon has persisted the bytes to disk. */
  inputAttachments?: AttachmentRef[];
  /** Wall-clock duration of the completed turn in milliseconds.
   *  Sourced from `TurnRecord.usage.durationMs`. Absent on streaming/
   *  interrupted turns and on providers that don't report it. */
  durationMs?: number;
  /** Raw provider-level model id used for this turn's reply. Sourced
   *  from `TurnRecord.usage.model` when available, falling back to
   *  `SessionSummary.model` mid-stream. Drives the agent-message
   *  info popover and (via subagents[]) the subagent header. */
  model?: string;
  /** Provider kind for this session. Identical across turns of a
   *  session but threaded per-item so the UI layer never has to
   *  cross-reference the session cache. */
  providerKind?: ProviderKind;
  /** Subagents spawned during this turn, carried forward so the
   *  per-subagent model can show up in the subagent box header. */
  subagents?: SubagentRecord[];
}

function formatTurnDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return `${minutes}m ${seconds}s`;
}

interface TurnViewProps {
  item: MessageItem;
  onOpenAttachment?: (attachment: AttachmentRef) => void;
}

function TurnViewInner({ item, onOpenAttachment }: TurnViewProps) {
  const { editStandalone } = useEditStandaloneSetting();
  const callsById = React.useMemo(() => {
    const map = new Map<string, ToolCall>();
    for (const tc of item.toolCalls ?? []) map.set(tc.callId, tc);
    return map;
  }, [item.toolCalls]);

  // Index subagents by the spawning Task's call id so ToolCallGroup
  // can look the record up directly when rendering a subagent box.
  // Identity matches `SubagentRecord.parentCallId` === the
  // dispatcher `ToolCall.callId`. Built once per turn-view render.
  const subagentsByParent = React.useMemo(() => {
    const map = new Map<string, SubagentRecord>();
    for (const rec of item.subagents ?? []) map.set(rec.parentCallId, rec);
    return map;
  }, [item.subagents]);

  const renderBlocks = React.useMemo(
    () => groupBlocks(item.blocks, callsById, { editStandalone }),
    [item.blocks, callsById, editStandalone],
  );

  // Index of the trailing text block in the grouped stream so the
  // blinking cursor only attaches to the very last text run while
  // the turn is still streaming.
  const lastTextRenderIdx = React.useMemo(() => {
    for (let i = renderBlocks.length - 1; i >= 0; i--) {
      if (renderBlocks[i].kind === "text") return i;
    }
    return -1;
  }, [renderBlocks]);

  const hasAnyContent = item.blocks.length > 0;

  return (
    <div className="space-y-3">
      {item.input !== null && (
        <>
          {/* Divider + "Restore to before" pill. Sits ABOVE the user
              message so it visually separates one exchange from the
              next and makes the rewind affordance the first thing
              the eye lands on when scanning the thread. The divider
              itself no-ops when we don't have a turn id yet (the
              streaming-echo row before turn_started lands). */}
          <RewindDivider turnId={item.turnId} />
          <UserMessage
            input={item.input}
            attachments={item.inputAttachments}
            onOpenAttachment={onOpenAttachment}
          />
        </>
      )}

      {!hasAnyContent && item.streaming && (
        <div className="text-sm text-muted-foreground">
          <span className="animate-pulse">Thinking…</span>
        </div>
      )}

      {renderBlocks.map((block, idx) => {
        switch (block.kind) {
          case "text":
            return (
              <AgentMessage
                key={block.key}
                output={block.text}
                streaming={item.streaming && idx === lastTextRenderIdx}
                status={item.status}
                model={item.model}
                providerKind={item.providerKind}
              />
            );
          case "reasoning":
            return (
              <details
                key={block.key}
                open
                className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
              >
                <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
                  Reasoning
                </summary>
                <p className="mt-2 whitespace-pre-wrap italic text-muted-foreground">
                  {block.text}
                </p>
              </details>
            );
          case "tool_call_group":
            return (
              <ToolCallGroup
                key={block.key}
                callIds={block.callIds}
                parentCallId={block.parentCallId}
                callsById={callsById}
                subagentsByParent={subagentsByParent}
                summary={block.summary}
                cardsDefaultOpen={block.cardsDefaultOpen}
              />
            );
          case "compact":
            return (
              <CompactBlock
                key={block.key}
                trigger={block.trigger}
                preTokens={block.preTokens}
                postTokens={block.postTokens}
                durationMs={block.durationMs}
                summary={block.summary}
              />
            );
          case "memory_recall":
            return (
              <MemoryRecallBlock
                key={block.key}
                mode={block.mode}
                memories={block.memories}
              />
            );
        }
      })}

      {/* Tail call latency — shows the wall-clock turn duration once the
          turn completes and the provider has reported usage.durationMs. */}
      {!item.streaming && item.durationMs != null && (
        <div className="flex items-center gap-1 text-[11px] text-muted-foreground/60">
          <Timer className="h-3 w-3" />
          <span className="tabular-nums">
            {formatTurnDuration(item.durationMs)}
          </span>
        </div>
      )}
    </div>
  );
}

export const TurnView = React.memo(TurnViewInner, (prev, next) => {
  if (prev.onOpenAttachment !== next.onOpenAttachment) return false;
  const a = prev.item;
  const b = next.item;
  return (
    a.turnId === b.turnId &&
    a.input === b.input &&
    a.status === b.status &&
    a.streaming === b.streaming &&
    a.durationMs === b.durationMs &&
    // Reference equality on both arrays — chat-view always builds new
    // arrays when blocks or tool calls change, so this catches every
    // streaming update.
    a.blocks === b.blocks &&
    a.toolCalls === b.toolCalls &&
    a.inputAttachments === b.inputAttachments &&
    // Model may upgrade mid-turn when ModelResolved promotes the
    // session alias to a pinned id; subagents[] grows as subagents
    // stream in and flips `model` when SubagentModelObserved fires.
    a.model === b.model &&
    a.providerKind === b.providerKind &&
    a.subagents === b.subagents
  );
});
