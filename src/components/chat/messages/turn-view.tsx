import * as React from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import type { AttachmentRef, ContentBlock, ToolCall, TurnStatus } from "@/lib/types";
import { ToolCallCard } from "../tool-call-card";
import { UserMessage } from "./user-message";
import { AgentMessage } from "./agent-message";

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
    };

function groupBlocks(
  blocks: ContentBlock[],
  callsById: Map<string, ToolCall>,
): RenderBlock[] {
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

      const parent = callsById.get(block.callId)?.parentCallId;

      if (parent === undefined) {
        // Main agent — sequential grouping.
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

    // Any non-tool block (text, reasoning) breaks the main-agent
    // streak. Sub-agent boxes are unaffected — they keep collecting
    // across these interruptions because their identity is the
    // parentCallId, not stream contiguity.
    currentMainGroup = null;
    if (block.kind === "text") {
      result.push({ kind: "text", text: block.text, key: `text-${idx}` });
    } else if (block.kind === "reasoning") {
      result.push({
        kind: "reasoning",
        text: block.text,
        key: `reasoning-${idx}`,
      });
    }
  });
  return result;
}

function ToolCallGroup({
  callIds,
  parentCallId,
  callsById,
}: {
  callIds: string[];
  parentCallId: string | undefined;
  callsById: Map<string, ToolCall>;
}) {
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
          <ToolCallCard key={tc.callId} toolCall={tc} />
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

    return (
      <details
        open
        className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
      >
        <summary className="cursor-pointer select-none text-[10px] font-medium uppercase tracking-wide text-muted-foreground hover:text-foreground">
          ↳ {agentLabel}{" "}
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
                <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded bg-muted/60 p-2 text-[11px] text-muted-foreground">
                  {parentCall.output}
                </pre>
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

  return (
    <details
      open
      className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
    >
      <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
        Tools <span className="text-muted-foreground/60">· {calls.length}</span>
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
}

interface TurnViewProps {
  item: MessageItem;
  onOpenAttachment?: (attachment: AttachmentRef) => void;
}

function TurnViewInner({ item, onOpenAttachment }: TurnViewProps) {
  const callsById = React.useMemo(() => {
    const map = new Map<string, ToolCall>();
    for (const tc of item.toolCalls ?? []) map.set(tc.callId, tc);
    return map;
  }, [item.toolCalls]);

  const renderBlocks = React.useMemo(
    () => groupBlocks(item.blocks, callsById),
    [item.blocks, callsById],
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
        <UserMessage
          input={item.input}
          attachments={item.inputAttachments}
          onOpenAttachment={onOpenAttachment}
        />
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
              />
            );
        }
      })}
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
    // Reference equality on both arrays — chat-view always builds new
    // arrays when blocks or tool calls change, so this catches every
    // streaming update.
    a.blocks === b.blocks &&
    a.toolCalls === b.toolCalls &&
    a.inputAttachments === b.inputAttachments
  );
});
