import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { GitBranch } from "lucide-react";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import type {
  ContentBlock,
  PermissionDecision,
  PermissionMode,
  ReasoningEffort,
  TurnRecord,
  UserInputAnswer,
  UserInputQuestion,
} from "@/lib/types";
import { connectStream, getGitBranch, sendMessage } from "@/lib/api";
import { cycleMode, MODE_LABELS } from "@/lib/mode-cycling";
import { toast } from "@/hooks/use-toast";
import { MessageList } from "./messages/message-list";
import { ChatInput } from "./chat-input";
import { PermissionPrompt } from "./permission-prompt";
import { QuestionPrompt } from "./question-prompt";
import { ChatToolbar } from "./chat-toolbar";
import { HeaderActions } from "./header-actions";
import { WorkingIndicator } from "./working-indicator";
import { StuckBanner } from "./stuck-banner";

// Trip the watchdog after this many seconds of silence while a tool
// call is pending. Picked to be well past a normal tool round-trip
// (even a slow Bash / Git command rarely exceeds 15–20s) but short
// enough that a user who just clicked Allow doesn't sit for a minute
// wondering if anything is happening.
const STUCK_TIMEOUT_MS = 45_000;

interface PermissionRequest {
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: string;
}

interface QuestionRequest {
  requestId: string;
  questions: UserInputQuestion[];
}

// Stream-order block accumulators. Adjacent text deltas coalesce into
// the trailing text block; a non-text block (e.g. a tool call) closes
// the run so the next text delta opens a new block. Always returns a
// new array so React.memo / reference equality picks up the change.
function appendTextDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "text") {
    return [...list.slice(0, -1), { kind: "text", text: last.text + delta }];
  }
  return [...list, { kind: "text", text: delta }];
}

function appendReasoningDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "reasoning") {
    return [
      ...list.slice(0, -1),
      { kind: "reasoning", text: last.text + delta },
    ];
  }
  return [...list, { kind: "reasoning", text: delta }];
}

export function ChatView({ sessionId }: { sessionId: string }) {
  const { state, dispatch } = useApp();
  const navigate = useNavigate();
  const [turns, setTurns] = React.useState<TurnRecord[]>([]);
  const [loading, setLoading] = React.useState(true);
  // FIFO queue of outstanding permission requests. Parallel tool
  // calls from the Claude Agent SDK fire multiple canUseTool
  // callbacks in the same turn, and each one hits the runtime as a
  // separate PermissionRequested event. Storing one at a time would
  // overwrite older prompts and leave their canUseTool Promises
  // blocking forever (the original "stuck pending" bug). The user
  // sees the head of the queue; clicking Allow/Deny pops it and the
  // next prompt slides in.
  const [pendingPermissions, setPendingPermissions] = React.useState<
    PermissionRequest[]
  >([]);
  const [pendingQuestion, setPendingQuestion] =
    React.useState<QuestionRequest | null>(null);
  const [effort, setEffort] = React.useState<ReasoningEffort>("high");
  const [permissionMode, setPermissionMode] =
    React.useState<PermissionMode>("accept_edits");
  const [pendingInput, setPendingInput] = React.useState<string | null>(null);
  // Watchdog state: `lastEventAt` bumps on every stream event for this
  // session so the 45s inactivity timer resets. `stuckSince` is set
  // when the timer fires and a pending tool call exists; rendering the
  // StuckBanner keys off it.
  const [lastEventAt, setLastEventAt] = React.useState<number>(() =>
    Date.now(),
  );
  const [stuckSince, setStuckSince] = React.useState<number | null>(null);

  const session = state.sessions.get(sessionId);
  const projectPath = React.useMemo(() => {
    if (!session?.projectId) return null;
    return state.projects.find((p) => p.projectId === session.projectId)?.path ?? null;
  }, [session?.projectId, state.projects]);
  const [gitBranch, setGitBranch] = React.useState<string | null>(null);

  React.useEffect(() => {
    if (!projectPath) {
      setGitBranch(null);
      return;
    }
    let cancelled = false;
    getGitBranch(projectPath).then((branch) => {
      if (!cancelled) setGitBranch(branch);
    });
    return () => {
      cancelled = true;
    };
  }, [projectPath]);

  // Keyboard shortcut for mode cycling (Shift+Tab)
  React.useEffect(() => {
    if (!session) return; // Only active when session exists

    const handleKeyDown = (event: KeyboardEvent) => {
      // Only respond to Shift+Tab
      if (event.key !== "Tab" || !event.shiftKey) return;

      // Don't interfere if user is typing in an input/textarea/contenteditable
      const target = event.target as HTMLElement;
      if (
        target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.isContentEditable
      ) {
        return;
      }

      // Prevent default Tab behavior (focus navigation)
      event.preventDefault();

      // Cycle to next mode
      const newMode = cycleMode(permissionMode, "forward");

      // Update local state
      setPermissionMode(newMode);

      // Send to daemon
      sendMessage({
        type: "update_permission_mode",
        session_id: sessionId,
        permission_mode: newMode,
      }).catch((err) => {
        console.error("Failed to update permission mode", err);
      });

      // Show toast notification
      toast({
        description: `Mode: ${MODE_LABELS[newMode]}`,
        duration: 2000,
      });
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [sessionId, session, permissionMode]);

  // Escape interrupts the in-flight turn. Mirrors the "esc" hint shown in
  // the working indicator. The title-rename Escape handler is scoped to
  // its own input element, so this window-level listener doesn't clobber
  // it when a rename is in progress.
  React.useEffect(() => {
    if (!session) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (session.status !== "running") return;
      event.preventDefault();
      sendMessage({ type: "interrupt_turn", session_id: sessionId }).catch(
        (err) => {
          console.error("Failed to interrupt turn", err);
        },
      );
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [sessionId, session]);

  // Set active session
  React.useEffect(() => {
    dispatch({ type: "set_active_session", sessionId });
    return () => {
      dispatch({ type: "set_active_session", sessionId: null });
    };
  }, [sessionId, dispatch]);

  // Load session detail
  React.useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setTurns([]);
    setPendingPermissions([]);
    setPendingQuestion(null);
    setPendingInput(null);
    setLastEventAt(Date.now());
    setStuckSince(null);

    sendMessage({ type: "load_session", session_id: sessionId }).then((res) => {
      if (cancelled) return;
      if (res && res.type === "session_loaded") {
        setTurns(res.session.turns);
      }
      setLoading(false);
    });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  // Listen for session-specific events via a dedicated stream
  React.useEffect(() => {
    let active = true;

    connectStream((message) => {
      if (!active) return;
      // SessionLoaded arrives outside the event stream — both as a
      // direct response to load_session and as a daemon-pushed reseed
      // when the broadcast subscriber lagged and dropped events. In
      // both cases, if it's for the active session we treat its
      // turns[] as authoritative and replace local state. This is the
      // recovery path for tool calls that would otherwise be stuck on
      // pending after a dropped tool_call_completed event.
      if (message.type === "session_loaded") {
        if (message.session.summary.sessionId === sessionId) {
          setTurns(message.session.turns);
          setPendingInput(null);
          setLastEventAt(Date.now());
          setStuckSince(null);
        }
        return;
      }
      if (message.type !== "event") return;
      const event = message.event;

      if (!("session_id" in event) || event.session_id !== sessionId) return;

      // Any event for this session proves the backend is still
      // talking. Reset the stuck-watchdog timer on every tick so it
      // only fires after genuine silence.
      setLastEventAt(Date.now());
      setStuckSince(null);

      switch (event.type) {
        case "turn_started":
          // Push the partial turn (with input set, output empty) so the
          // user message becomes visible immediately. The optimistic
          // pending row covers the gap between sendMessage being called
          // and this event arriving — clear it now.
          setPendingInput(null);
          setTurns((prev) => {
            const exists = prev.some((t) => t.turnId === event.turn.turnId);
            if (exists) {
              return prev.map((t) =>
                t.turnId === event.turn.turnId ? event.turn : t,
              );
            }
            return [...prev, event.turn];
          });
          break;

        case "turn_completed":
          setPendingInput(null);
          setTurns((prev) => {
            const exists = prev.some((t) => t.turnId === event.turn.turnId);
            if (exists) {
              return prev.map((t) =>
                t.turnId === event.turn.turnId ? event.turn : t,
              );
            }
            return [...prev, event.turn];
          });
          break;

        case "content_delta": {
          // Extend the trailing text block of the matching turn with
          // the new delta, or push a new text block if the previous
          // block was something else (e.g. a tool call). This is what
          // preserves stream order in the rendered view.
          const deltaEvent = event;
          setTurns((prev) =>
            prev.map((t) =>
              t.turnId === deltaEvent.turn_id
                ? {
                    ...t,
                    output: deltaEvent.accumulated_output,
                    blocks: appendTextDelta(t.blocks, deltaEvent.delta),
                  }
                : t,
            ),
          );
          break;
        }

        case "reasoning_delta": {
          const deltaEvent = event;
          setTurns((prev) =>
            prev.map((t) =>
              t.turnId === deltaEvent.turn_id
                ? {
                    ...t,
                    reasoning: (t.reasoning ?? "") + deltaEvent.delta,
                    blocks: appendReasoningDelta(t.blocks, deltaEvent.delta),
                  }
                : t,
            ),
          );
          break;
        }

        case "tool_call_started": {
          // Append the new tool call to the matching turn so it appears
          // immediately rather than waiting for turn_completed. The
          // block records the stream position; toolCalls[] holds the
          // mutable status/output that tool_call_completed updates.
          const callEvent = event;
          setTurns((prev) =>
            prev.map((t) =>
              t.turnId === callEvent.turn_id
                ? {
                    ...t,
                    toolCalls: [
                      ...(t.toolCalls ?? []),
                      {
                        callId: callEvent.call_id,
                        name: callEvent.name,
                        args: callEvent.args,
                        status: "pending" as const,
                        parentCallId: callEvent.parent_call_id,
                      },
                    ],
                    blocks: [
                      ...(t.blocks ?? []),
                      { kind: "tool_call", callId: callEvent.call_id },
                    ],
                  }
                : t,
            ),
          );
          break;
        }

        case "tool_call_completed": {
          // Update the matching tool call with its output/error and
          // flip status. New arrays so memoization detects the change.
          // No blocks change — the block is just a position reference.
          const callEvent = event;
          setTurns((prev) =>
            prev.map((t) => {
              if (t.turnId !== callEvent.turn_id || !t.toolCalls) return t;
              return {
                ...t,
                toolCalls: t.toolCalls.map((tc) =>
                  tc.callId === callEvent.call_id
                    ? {
                        ...tc,
                        output: callEvent.output,
                        error: callEvent.error,
                        status: callEvent.error
                          ? ("failed" as const)
                          : ("completed" as const),
                      }
                    : tc,
                ),
              };
            }),
          );
          break;
        }

        case "user_question_asked":
          setPendingQuestion({
            requestId: event.request_id,
            questions: event.questions,
          });
          break;

        case "permission_requested":
          // Append to the FIFO queue. Dedupe on request_id because
          // the daemon-side lag-recovery path can replay events, and
          // a parallel canUseTool burst must not create N copies of
          // the same prompt. The head of the queue is the prompt the
          // user currently sees.
          setPendingPermissions((prev) => {
            if (prev.some((p) => p.requestId === event.request_id)) {
              return prev;
            }
            return [
              ...prev,
              {
                requestId: event.request_id,
                toolName: event.tool_name,
                input: event.input,
                suggested: event.suggested,
              },
            ];
          });
          break;

        case "session_deleted":
        case "session_archived":
          // The active thread was deleted or archived from the sidebar
          // (or another window). Get out of the chat view so the user
          // isn't staring at a stale title with no data behind it.
          navigate({ to: "/" });
          break;
      }
    });

    return () => {
      active = false;
    };
  }, [sessionId, navigate]);

  async function handleSend(input: string) {
    // Optimistic: show the user's message immediately, then await the
    // round-trip. turn_started will clear this and replace it with the
    // real turn from the daemon.
    setPendingInput(input);
    try {
      await sendMessage({
        type: "send_turn",
        session_id: sessionId,
        input,
        permission_mode: permissionMode,
        reasoning_effort: effort,
      });
    } catch (err) {
      setPendingInput(null);
      throw err;
    }
  }

  async function handleInterrupt() {
    await sendMessage({ type: "interrupt_turn", session_id: sessionId });
  }

  async function handlePermissionDecision(
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
  ) {
    // Always act on the head of the queue — that's what the user
    // just clicked on. Pop it before the await so a rapid double
    // click can't answer the same request twice, and so the next
    // queued prompt becomes visible immediately.
    const head = pendingPermissions[0];
    if (!head) return;
    setPendingPermissions((prev) => prev.slice(1));
    await sendMessage({
      type: "answer_permission",
      session_id: sessionId,
      request_id: head.requestId,
      decision,
      ...(modeOverride ? { permission_mode_override: modeOverride } : {}),
    });
    if (modeOverride) {
      // Mirror the chosen mode into local state so the toolbar dropdown
      // and the next send_turn pick it up. The Claude SDK side already
      // applies the mode via the bundled updatedPermissions, so this is
      // purely a UI sync — no second daemon round-trip.
      setPermissionMode(modeOverride);
    }
  }

  async function handleQuestionSubmit(answers: UserInputAnswer[]) {
    if (!pendingQuestion) return;
    const requestId = pendingQuestion.requestId;
    setPendingQuestion(null);
    await sendMessage({
      type: "answer_question",
      session_id: sessionId,
      request_id: requestId,
      answers,
    });
  }

  async function handleQuestionCancel() {
    if (!pendingQuestion) return;
    const requestId = pendingQuestion.requestId;
    setPendingQuestion(null);
    await sendMessage({
      type: "cancel_question",
      session_id: sessionId,
      request_id: requestId,
    });
  }

  const isRunning = session?.status === "running";
  // The in-flight turn (if any). Used to drive the WorkingIndicator's
  // elapsed-time clock from the daemon-side createdAt timestamp so the
  // counter doesn't drift between client and server.
  const runningTurn = React.useMemo(() => {
    if (!isRunning) return null;
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].status === "running") return turns[i];
    }
    return null;
  }, [isRunning, turns]);

  // Is there at least one tool call on the running turn still waiting
  // for its completion event? That's the precondition for the
  // stuck-watchdog: we don't care about ordinary model thinking
  // latency, only about cases where a tool is visibly in "pending"
  // and nothing is moving.
  const hasPendingToolCall = React.useMemo(() => {
    if (!runningTurn) return false;
    return (runningTurn.toolCalls ?? []).some((tc) => tc.status === "pending");
  }, [runningTurn]);

  // Arm the stuck-watchdog. We only trip it when the session is
  // running *and* at least one tool call is pending, so idle
  // pre-tool "Thinking…" periods don't falsely flag as stuck. The
  // timer is rearmed by `lastEventAt` bumping on each event.
  React.useEffect(() => {
    if (!isRunning || !hasPendingToolCall) {
      setStuckSince(null);
      return;
    }
    const now = Date.now();
    const elapsed = now - lastEventAt;
    if (elapsed >= STUCK_TIMEOUT_MS) {
      setStuckSince(lastEventAt);
      return;
    }
    const id = setTimeout(() => {
      setStuckSince(lastEventAt);
    }, STUCK_TIMEOUT_MS - elapsed);
    return () => clearTimeout(id);
  }, [isRunning, hasPendingToolCall, lastEventAt]);

  const title = session?.title || "New thread";

  const [editingTitle, setEditingTitle] = React.useState(false);
  const [titleDraft, setTitleDraft] = React.useState(title);
  const titleInputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    setTitleDraft(title);
  }, [title]);

  React.useEffect(() => {
    if (editingTitle) {
      titleInputRef.current?.focus();
      titleInputRef.current?.select();
    }
  }, [editingTitle]);

  function commitTitleRename() {
    const trimmed = titleDraft.trim();
    setEditingTitle(false);
    if (trimmed && trimmed !== title) {
      sendMessage({
        type: "rename_session",
        session_id: sessionId,
        title: trimmed,
      });
    }
  }

  const toolbar = session ? (
    <ChatToolbar
      sessionId={sessionId}
      provider={session.provider}
      currentModel={session.model}
      effort={effort}
      onEffortChange={setEffort}
      permissionMode={permissionMode}
      onPermissionModeChange={setPermissionMode}
    />
  ) : null;

  return (
    <div className="flex h-svh flex-col">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <div className="flex min-w-0 flex-col leading-tight">
          {editingTitle ? (
            <input
              ref={titleInputRef}
              className="min-w-0 truncate rounded border border-input bg-background px-1.5 py-0.5 text-sm font-medium outline-none"
              value={titleDraft}
              onChange={(e) => setTitleDraft(e.target.value)}
              onBlur={commitTitleRename}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitTitleRename();
                if (e.key === "Escape") {
                  setTitleDraft(title);
                  setEditingTitle(false);
                }
              }}
            />
          ) : (
            <span
              className="cursor-pointer truncate font-medium hover:text-muted-foreground"
              onClick={() => setEditingTitle(true)}
            >
              {title}
            </span>
          )}
          {gitBranch && (
            <span className="inline-flex items-center gap-1 truncate text-[11px] text-muted-foreground">
              <GitBranch className="h-3 w-3 shrink-0" />
              {gitBranch}
            </span>
          )}
        </div>
        <div className="ml-auto flex items-center gap-2">
          <HeaderActions />
        </div>
      </header>

      <MessageList
        turns={turns}
        loading={loading}
        pendingInput={pendingInput}
      />

      {isRunning && session && runningTurn && (
        <WorkingIndicator
          provider={session.provider}
          startedAt={runningTurn.createdAt}
          onInterrupt={handleInterrupt}
        />
      )}

      {pendingQuestion && (
        <QuestionPrompt
          questions={pendingQuestion.questions}
          onSubmit={handleQuestionSubmit}
          onCancel={handleQuestionCancel}
        />
      )}

      {pendingPermissions.length > 0 && (
        <PermissionPrompt
          // Head-of-queue. The `key` forces React to remount the
          // prompt so any local component state (e.g. the plan-exit
          // mode picker's `pending` flag) resets between queued
          // prompts and the user can't accidentally double-answer
          // the next one with stale state.
          key={pendingPermissions[0].requestId}
          toolName={pendingPermissions[0].toolName}
          input={pendingPermissions[0].input}
          onDecision={handlePermissionDecision}
          queueDepth={pendingPermissions.length}
        />
      )}

      {stuckSince !== null && pendingPermissions.length === 0 && !pendingQuestion && (
        <StuckBanner
          elapsedSeconds={Math.floor((Date.now() - stuckSince) / 1000)}
          onInterrupt={() => {
            setStuckSince(null);
            handleInterrupt();
          }}
          onReload={() => {
            setStuckSince(null);
            sendMessage({ type: "load_session", session_id: sessionId });
          }}
        />
      )}

      <ChatInput
        onSend={handleSend}
        onInterrupt={handleInterrupt}
        sessionStatus={session?.status}
        disabled={loading}
        toolbar={toolbar}
      />
    </div>
  );
}
