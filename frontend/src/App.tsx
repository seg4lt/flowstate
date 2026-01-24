import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "./components/ui/button";
import { Badge } from "./components/ui/badge";
import { Separator } from "./components/ui/separator";
import { ScrollArea } from "./components/ui/scroll-area";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "./components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "./components/ui/tooltip";
import { Plus, RefreshCw, Settings, X, Minus, Square, Send, MessageSquare, Bot, Trash2 } from "lucide-react";

type ProviderKind = "codex" | "claude" | "github_copilot";
type ProviderStatusLevel = "ready" | "warning" | "error";
type SessionStatus = "ready" | "running" | "interrupted";
type TurnStatus = "running" | "completed" | "interrupted" | "failed";
type ToolCallStatus = "pending" | "completed" | "failed";
type PermissionMode = "default" | "accept_edits" | "plan" | "bypass";
type PermissionDecision = "allow" | "allow_always" | "deny" | "deny_always";
type FileOperation = "write" | "edit" | "delete";
type SubagentStatus = "running" | "completed" | "failed";
type PlanStatus = "proposed" | "accepted" | "rejected";

interface ToolCall {
  callId: string;
  name: string;
  args: unknown;
  output?: string;
  error?: string;
  status: ToolCallStatus;
}

interface FileChangeRecord {
  callId: string;
  path: string;
  operation: FileOperation;
  before?: string;
  after?: string;
}

interface PlanStep {
  title: string;
  detail?: string;
}

interface PlanRecord {
  planId: string;
  title: string;
  steps: PlanStep[];
  raw: string;
  status: PlanStatus;
}

interface SubagentRecord {
  agentId: string;
  parentCallId: string;
  agentType: string;
  prompt: string;
  events?: unknown[];
  output?: string;
  error?: string;
  status: SubagentStatus;
}

interface PendingPermission {
  sessionId: string;
  turnId: string;
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: PermissionDecision;
}

interface PendingQuestion {
  sessionId: string;
  turnId: string;
  requestId: string;
  question: string;
}

interface ProviderModel {
  value: string;
  label: string;
}

interface ProviderStatus {
  kind: ProviderKind;
  label: string;
  installed: boolean;
  authenticated: boolean;
  version: string | null;
  status: ProviderStatusLevel;
  message: string | null;
  models: ProviderModel[];
}

interface TurnRecord {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  reasoning?: string;
  toolCalls?: ToolCall[];
  fileChanges?: FileChangeRecord[];
  subagents?: SubagentRecord[];
  plan?: PlanRecord;
  permissionMode?: PermissionMode;
  createdAt: string;
  updatedAt: string;
  /** Local-only: set on optimistic-echo turns, cleared when server version replaces it. */
  pendingId?: string;
}

interface SessionSummary {
  sessionId: string;
  provider: ProviderKind;
  title: string;
  status: SessionStatus;
  createdAt: string;
  updatedAt: string;
  lastTurnPreview: string | null;
  turnCount: number;
  model?: string;
}

interface SessionDetail {
  summary: SessionSummary;
  turns: TurnRecord[];
}

interface AppSnapshot {
  generatedAt: string;
  sessions: SessionDetail[];
}

interface BootstrapPayload {
  appName: string;
  generatedAt: string;
  wsUrl: string;
  providers: ProviderStatus[];
  snapshot: AppSnapshot;
}

// RuntimeEvent fields use snake_case to match Rust serde serialization.
type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; session_id: string; turn: TurnRecord }
  | { type: "content_delta"; session_id: string; turn_id: string; delta: string; accumulated_output: string }
  | { type: "reasoning_delta"; session_id: string; turn_id: string; delta: string }
  | { type: "tool_call_started"; session_id: string; turn_id: string; call_id: string; name: string; args: unknown }
  | { type: "tool_call_completed"; session_id: string; turn_id: string; call_id: string; output: string; error?: string }
  | { type: "turn_completed"; session_id: string; session: SessionSummary; turn: TurnRecord }
  | { type: "session_interrupted"; session: SessionSummary; message: string }
  | { type: "session_deleted"; session_id: string }
  | {
      type: "permission_requested";
      session_id: string;
      turn_id: string;
      request_id: string;
      tool_name: string;
      input: unknown;
      suggested: PermissionDecision;
    }
  | {
      type: "user_question_asked";
      session_id: string;
      turn_id: string;
      request_id: string;
      question: string;
    }
  | {
      type: "file_changed";
      session_id: string;
      turn_id: string;
      call_id: string;
      path: string;
      operation: FileOperation;
      before?: string;
      after?: string;
    }
  | {
      type: "subagent_started";
      session_id: string;
      turn_id: string;
      parent_call_id: string;
      agent_id: string;
      agent_type: string;
      prompt: string;
    }
  | {
      type: "subagent_event";
      session_id: string;
      turn_id: string;
      agent_id: string;
      event: unknown;
    }
  | {
      type: "subagent_completed";
      session_id: string;
      turn_id: string;
      agent_id: string;
      output: string;
      error?: string;
    }
  | {
      type: "plan_proposed";
      session_id: string;
      turn_id: string;
      plan_id: string;
      title: string;
      steps: PlanStep[];
      raw: string;
    }
  | { type: "error"; message: string }
  | { type: "info"; message: string }
  | {
      type: "provider_models_updated";
      provider: ProviderKind;
      models: ProviderModel[];
    };

type ServerMessage =
  | { type: "welcome"; bootstrap: BootstrapPayload }
  | { type: "snapshot"; snapshot: AppSnapshot }
  | { type: "session_created"; session: SessionSummary }
  | { type: "pong" }
  | { type: "ack"; message: string }
  | { type: "event"; event: RuntimeEvent }
  | { type: "error"; message: string };

const EMPTY_SNAPSHOT: AppSnapshot = {
  generatedAt: new Date(0).toISOString(),
  sessions: [],
};

const PROVIDER_COLORS: Record<ProviderKind, string> = {
  codex: "bg-emerald-500",
  claude: "bg-amber-500",
  github_copilot: "bg-blue-500",
};

const PROVIDER_LABELS: Record<ProviderKind, string> = {
  codex: "Codex",
  claude: "Claude",
  github_copilot: "GitHub Copilot",
};


// Traffic light component
function TrafficLights() {
  const sendCommand = (cmd: string) => {
    // @ts-ignore - wry IPC
    if (window.ipc?.postMessage) {
      // @ts-ignore
      window.ipc.postMessage(JSON.stringify({ cmd }));
    }
  };

  return (
    <div className="flex gap-2">
      <button
        className="w-3 h-3 rounded-full bg-[#ff5f57] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("close")}
        aria-label="Close"
      >
        <X className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#febc2e] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("minimize")}
        aria-label="Minimize"
      >
        <Minus className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#28c840] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => sendCommand("maximize")}
        aria-label="Maximize"
      >
        <Square className="w-1.5 h-1.5 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
    </div>
  );
}

// Title Bar Component
function TitleBar() {
  return (
    <div className="h-[38px] bg-secondary border-b border-border flex items-center px-4 titlebar shrink-0">
      <div className="flex items-center gap-3 titlebar-content w-full">
        <TrafficLights />
        <div className="flex items-center gap-2 ml-2">
          <span className="font-semibold text-sm">ZenUI</span>
          <Badge variant="secondary" className="text-[10px] h-4 px-1.5">Alpha</Badge>
        </div>
        <div className="flex-1" />
        <div className="flex items-center gap-1">
          <Tooltip>
            <TooltipTrigger>
              <Button variant="ghost" size="icon" className="h-7 w-7">
                <Settings className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Settings</TooltipContent>
          </Tooltip>
        </div>
      </div>
    </div>
  );
}

// Thread Item Component
function ThreadItem({
  session,
  isActive,
  onClick,
  onDelete,
}: {
  session: SessionDetail;
  isActive: boolean;
  onClick: () => void;
  onDelete: () => void;
}) {
  const { summary } = session;
  const isRunning = summary.status === "running";

  return (
    <div
      className={`group w-full flex items-center gap-2 px-2 py-1.5 rounded-md transition-colors cursor-pointer ${
        isActive
          ? "bg-accent text-accent-foreground"
          : "hover:bg-muted text-muted-foreground hover:text-foreground"
      }`}
      onClick={onClick}
    >
      <div className={`w-2 h-2 rounded-full shrink-0 ${PROVIDER_COLORS[summary.provider]}`} />
      <div className="flex-1 min-w-0">
        <div className="text-sm font-medium truncate">{summary.title}</div>
        <div className="text-xs text-muted-foreground truncate">
          {isRunning ? (
            <span className="flex items-center gap-1">
              <span className="w-1.5 h-1.5 rounded-full bg-yellow-500 animate-pulse" />
              Running...
            </span>
          ) : (
            summary.lastTurnPreview || "No messages yet"
          )}
        </div>
      </div>
      <button
        className="opacity-0 group-hover:opacity-100 transition-opacity p-0.5 rounded hover:bg-destructive/20 hover:text-destructive shrink-0"
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        aria-label="Delete thread"
      >
        <Trash2 className="w-3 h-3" />
      </button>
    </div>
  );
}

// Provider Status Component
function ProviderStatusItem({ status }: { status: ProviderStatus }) {
  const statusColors: Record<ProviderStatusLevel, string> = {
    ready: "bg-green-500",
    warning: "bg-yellow-500",
    error: "bg-red-500",
  };

  return (
    <div className="flex items-center gap-2 px-2 py-1.5 rounded-md">
      <div className={`w-2 h-2 rounded-full ${statusColors[status.status]}`} />
      <div className="flex-1 min-w-0">
        <div className="text-sm font-medium">{status.label}</div>
        <div className="text-xs text-muted-foreground truncate">
          {status.installed ? (status.authenticated ? "Ready" : "Not authenticated") : "Not installed"}
        </div>
      </div>
    </div>
  );
}

const PERMISSION_MODE_LABELS: Record<PermissionMode, string> = {
  accept_edits: "Accept edits",
  default: "Prompt all",
  plan: "Plan mode",
  bypass: "Bypass",
};

// Main App
export default function App() {
  const [bootstrap, setBootstrap] = useState<BootstrapPayload | null>(null);
  const [snapshot, setSnapshot] = useState<AppSnapshot>(EMPTY_SNAPSHOT);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [permissionMode, setPermissionMode] = useState<PermissionMode>("accept_edits");
  const [pendingPermissions, setPendingPermissions] = useState<PendingPermission[]>([]);
  const [pendingQuestion, setPendingQuestion] = useState<PendingQuestion | null>(null);
  const [questionDraft, setQuestionDraft] = useState("");
  const [connectionStatus, setConnectionStatus] = useState<"connected" | "connecting" | "disconnected">("connecting");
  const [lastAction, setLastAction] = useState<string>("Ready");
  const socketRef = useRef<WebSocket | null>(null);

  const activeSession = useMemo(
    () => snapshot.sessions.find((s) => s.summary.sessionId === activeSessionId) ?? null,
    [activeSessionId, snapshot.sessions]
  );

  function send(message: unknown) {
    const socket = socketRef.current;
    console.log("Send called, socket state:", socket?.readyState);
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      console.error("WebSocket not connected");
      return;
    }
    const payload = JSON.stringify(message);
    console.log("Sending:", payload);
    socket.send(payload);
  }

  async function loadSnapshot() {
    try {
      const response = await fetch("/api/snapshot");
      if (!response.ok) throw new Error(`Failed: ${response.status}`);
      return (await response.json()) as AppSnapshot;
    } catch (error) {
      console.error("Snapshot error:", error);
      return EMPTY_SNAPSHOT;
    }
  }

  async function refreshSnapshot(preferredSessionId?: string | null) {
    const nextSnapshot = await loadSnapshot();
    setSnapshot(nextSnapshot);
    if (preferredSessionId && nextSnapshot.sessions.some((s) => s.summary.sessionId === preferredSessionId)) {
      setActiveSessionId(preferredSessionId);
    } else if (nextSnapshot.sessions.length > 0 && !activeSessionId) {
      setActiveSessionId(nextSnapshot.sessions[0].summary.sessionId);
    }
  }

  useEffect(() => {
    let disposed = false;

    async function boot() {
      try {
        const response = await fetch("/api/bootstrap");
        if (!response.ok) throw new Error(`Bootstrap failed: ${response.status}`);

        const nextBootstrap = (await response.json()) as BootstrapPayload;
        if (disposed) return;

        setBootstrap(nextBootstrap);
        setSnapshot(nextBootstrap.snapshot);
        if (nextBootstrap.snapshot.sessions.length > 0) {
          setActiveSessionId(nextBootstrap.snapshot.sessions[0].summary.sessionId);
        }

        const socket = new WebSocket(nextBootstrap.wsUrl);
        socketRef.current = socket;

        socket.addEventListener("open", () => {
          if (disposed) return;
          setConnectionStatus("connected");
        });

        socket.addEventListener("close", () => {
          if (disposed) return;
          setConnectionStatus("disconnected");
        });

        socket.addEventListener("message", (event) => {
          if (disposed) return;
          const payload = JSON.parse(event.data) as ServerMessage;

          switch (payload.type) {
            case "welcome":
              setBootstrap(payload.bootstrap);
              setSnapshot(payload.bootstrap.snapshot);
              break;
            case "snapshot":
              setSnapshot(payload.snapshot);
              break;
            case "session_created":
              setLastAction(`Session created: ${payload.session.title}`);
              refreshSnapshot(payload.session.sessionId);
              break;
            case "event": {
              const ev = payload.event;

              const matchesRunning = (t: TurnRecord, turnId: string) =>
                t.turnId === turnId || t.pendingId !== undefined;

              if (ev.type === "content_delta") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) =>
                            !matchesRunning(t, ev.turn_id)
                              ? t
                              : { ...t, output: ev.accumulated_output }
                          ),
                        }
                  ),
                }));
              } else if (ev.type === "reasoning_delta") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) =>
                            !matchesRunning(t, ev.turn_id)
                              ? t
                              : { ...t, reasoning: (t.reasoning ?? "") + ev.delta }
                          ),
                        }
                  ),
                }));
              } else if (ev.type === "tool_call_started") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) =>
                            !matchesRunning(t, ev.turn_id)
                              ? t
                              : {
                                  ...t,
                                  toolCalls: [
                                    ...(t.toolCalls ?? []),
                                    { callId: ev.call_id, name: ev.name, args: ev.args, status: "pending" as ToolCallStatus },
                                  ],
                                }
                          ),
                        }
                  ),
                }));
              } else if (ev.type === "tool_call_completed") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) =>
                            !matchesRunning(t, ev.turn_id)
                              ? t
                              : {
                                  ...t,
                                  toolCalls: (t.toolCalls ?? []).map((tc) =>
                                    tc.callId !== ev.call_id
                                      ? tc
                                      : {
                                          ...tc,
                                          output: ev.output,
                                          error: ev.error,
                                          status: (ev.error ? "failed" : "completed") as ToolCallStatus,
                                        }
                                  ),
                                }
                          ),
                        }
                  ),
                }));
              } else if (ev.type === "turn_started") {
                // Stamp the optimistic-echo turn with the real server turn_id.
                // Do NOT refresh the snapshot — that would race against incoming deltas.
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) => {
                    if (s.summary.sessionId !== ev.session_id) return s;
                    const hasPending = s.turns.some((t) => t.pendingId !== undefined);
                    if (hasPending) {
                      return {
                        ...s,
                        turns: s.turns.map((t) =>
                          t.pendingId === undefined
                            ? t
                            : { ...t, turnId: ev.turn.turnId, pendingId: undefined }
                        ),
                      };
                    }
                    // No optimistic echo (e.g. turn started by something other than this client)
                    return { ...s, turns: [...s.turns, ev.turn] };
                  }),
                }));
              } else if (ev.type === "turn_completed") {
                // Merge final state in place — no full snapshot refresh.
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) => {
                    if (s.summary.sessionId !== ev.session_id) return s;
                    return {
                      ...s,
                      summary: { ...s.summary, ...ev.session },
                      turns: s.turns.map((t) =>
                        t.turnId !== ev.turn.turnId
                          ? t
                          : {
                              ...t,
                              status: ev.turn.status,
                              output: ev.turn.output,
                              updatedAt: ev.turn.updatedAt,
                              reasoning: ev.turn.reasoning ?? t.reasoning,
                              toolCalls: ev.turn.toolCalls ?? t.toolCalls,
                            }
                      ),
                    };
                  }),
                }));
              } else if (ev.type === "permission_requested") {
                setPendingPermissions((prev) => [
                  ...prev,
                  {
                    sessionId: ev.session_id,
                    turnId: ev.turn_id,
                    requestId: ev.request_id,
                    toolName: ev.tool_name,
                    input: ev.input,
                    suggested: ev.suggested,
                  },
                ]);
              } else if (ev.type === "user_question_asked") {
                setPendingQuestion({
                  sessionId: ev.session_id,
                  turnId: ev.turn_id,
                  requestId: ev.request_id,
                  question: ev.question,
                });
                setQuestionDraft("");
              } else if (ev.type === "file_changed") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) => {
                            if (!matchesRunning(t, ev.turn_id)) return t;
                            const fc: FileChangeRecord = {
                              callId: ev.call_id,
                              path: ev.path,
                              operation: ev.operation,
                              before: ev.before,
                              after: ev.after,
                            };
                            const existing = t.fileChanges ?? [];
                            const idx = existing.findIndex((x) => x.callId === ev.call_id);
                            const next =
                              idx >= 0
                                ? existing.map((x, i) => (i === idx ? fc : x))
                                : [...existing, fc];
                            return { ...t, fileChanges: next };
                          }),
                        }
                  ),
                }));
              } else if (ev.type === "subagent_started") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) => {
                            if (!matchesRunning(t, ev.turn_id)) return t;
                            const existing = t.subagents ?? [];
                            if (existing.some((x) => x.agentId === ev.agent_id)) return t;
                            const sub: SubagentRecord = {
                              agentId: ev.agent_id,
                              parentCallId: ev.parent_call_id,
                              agentType: ev.agent_type,
                              prompt: ev.prompt,
                              events: [],
                              status: "running",
                            };
                            return { ...t, subagents: [...existing, sub] };
                          }),
                        }
                  ),
                }));
              } else if (ev.type === "subagent_event") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) => {
                            if (!matchesRunning(t, ev.turn_id)) return t;
                            const existing = t.subagents ?? [];
                            return {
                              ...t,
                              subagents: existing.map((x) =>
                                x.agentId !== ev.agent_id
                                  ? x
                                  : { ...x, events: [...(x.events ?? []), ev.event] }
                              ),
                            };
                          }),
                        }
                  ),
                }));
              } else if (ev.type === "subagent_completed") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) => {
                            if (!matchesRunning(t, ev.turn_id)) return t;
                            const existing = t.subagents ?? [];
                            return {
                              ...t,
                              subagents: existing.map((x) =>
                                x.agentId !== ev.agent_id
                                  ? x
                                  : {
                                      ...x,
                                      output: ev.output,
                                      error: ev.error,
                                      status: ev.error ? "failed" : "completed",
                                    }
                              ),
                            };
                          }),
                        }
                  ),
                }));
              } else if (ev.type === "plan_proposed") {
                setSnapshot((prev) => ({
                  ...prev,
                  sessions: prev.sessions.map((s) =>
                    s.summary.sessionId !== ev.session_id
                      ? s
                      : {
                          ...s,
                          turns: s.turns.map((t) => {
                            if (!matchesRunning(t, ev.turn_id)) return t;
                            const plan: PlanRecord = {
                              planId: ev.plan_id,
                              title: ev.title,
                              steps: ev.steps,
                              raw: ev.raw,
                              status: "proposed",
                            };
                            return { ...t, plan };
                          }),
                        }
                  ),
                }));
              } else if (ev.type === "session_started") {
                refreshSnapshot();
              } else if (ev.type === "session_deleted") {
                setSnapshot((prev) => {
                  const remaining = prev.sessions.filter(
                    (s) => s.summary.sessionId !== ev.session_id
                  );
                  return { ...prev, sessions: remaining };
                });
                setActiveSessionId((cur) =>
                  cur === ev.session_id ? null : cur
                );
              } else if (ev.type === "error") {
                setLastAction(`Error: ${ev.message}`);
              } else if (ev.type === "provider_models_updated") {
                // eslint-disable-next-line no-console
                console.log("[zenui] provider_models_updated", ev.provider, ev.models);
                setBootstrap((prev) => {
                  if (!prev) {
                    // eslint-disable-next-line no-console
                    console.warn("[zenui] provider_models_updated arrived before bootstrap was set; dropping event");
                    return prev;
                  }
                  return {
                    ...prev,
                    providers: prev.providers.map((p) =>
                      p.kind === ev.provider ? { ...p, models: ev.models } : p
                    ),
                  };
                });
                setLastAction(
                  `Refreshed ${PROVIDER_LABELS[ev.provider]} models (${ev.models.length})`
                );
              }
              break;
            }
          }
        });
      } catch (error) {
        console.error("Bootstrap error:", error);
        setConnectionStatus("disconnected");
      }
    }

    boot();

    return () => {
      disposed = true;
      socketRef.current?.close();
      socketRef.current = null;
    };
  }, []);

  const startSession = (provider: ProviderKind, model?: string) => {
    setLastAction(`Creating ${PROVIDER_LABELS[provider]} session...`);
    send({ type: "start_session", provider, title: null, model: model ?? null });
  };

  const sendTurn = () => {
    if (!activeSession || !prompt.trim()) return;
    const input = prompt.trim();

    // Optimistic echo: splice a synthetic running turn into state immediately.
    const pendingId = crypto.randomUUID();
    const now = new Date().toISOString();
    const syntheticTurn: TurnRecord = {
      turnId: pendingId,
      input,
      output: "",
      status: "running",
      createdAt: now,
      updatedAt: now,
      pendingId,
    };
    setSnapshot((prev) => ({
      ...prev,
      sessions: prev.sessions.map((s) =>
        s.summary.sessionId !== activeSession.summary.sessionId
          ? s
          : {
              summary: {
                ...s.summary,
                status: "running",
                turnCount: s.summary.turnCount + 1,
                lastTurnPreview: input.slice(0, 80),
              },
              turns: [...s.turns, syntheticTurn],
            }
      ),
    }));

    send({
      type: "send_turn",
      session_id: activeSession.summary.sessionId,
      input,
      permission_mode: permissionMode,
    });
    setPrompt("");
  };

  const deleteSession = (sessionId: string) => {
    // window.confirm is unsupported in wry's WKWebView (no WKUIDelegate handler),
    // so it returns false silently. Send the delete directly instead.
    send({ type: "delete_session", session_id: sessionId });
  };

  const answerPermission = (req: PendingPermission, decision: PermissionDecision) => {
    send({
      type: "answer_permission",
      session_id: req.sessionId,
      request_id: req.requestId,
      decision,
    });
    setPendingPermissions((prev) => prev.filter((p) => p.requestId !== req.requestId));
  };

  const submitQuestionAnswer = () => {
    if (!pendingQuestion || !questionDraft.trim()) return;
    send({
      type: "answer_question",
      session_id: pendingQuestion.sessionId,
      request_id: pendingQuestion.requestId,
      answer: questionDraft.trim(),
    });
    setPendingQuestion(null);
    setQuestionDraft("");
  };

  const acceptPlan = (sessionId: string, planId: string) => {
    send({ type: "accept_plan", session_id: sessionId, plan_id: planId });
    setSnapshot((prev) => ({
      ...prev,
      sessions: prev.sessions.map((s) =>
        s.summary.sessionId !== sessionId
          ? s
          : {
              ...s,
              turns: s.turns.map((t) =>
                t.plan?.planId === planId
                  ? { ...t, plan: { ...t.plan, status: "accepted" as PlanStatus } }
                  : t
              ),
            }
      ),
    }));
  };

  const rejectPlan = (sessionId: string, planId: string) => {
    send({ type: "reject_plan", session_id: sessionId, plan_id: planId });
    setSnapshot((prev) => ({
      ...prev,
      sessions: prev.sessions.map((s) =>
        s.summary.sessionId !== sessionId
          ? s
          : {
              ...s,
              turns: s.turns.map((t) =>
                t.plan?.planId === planId
                  ? { ...t, plan: { ...t.plan, status: "rejected" as PlanStatus } }
                  : t
              ),
            }
      ),
    }));
  };

  return (
    <div className="relative h-screen w-screen flex bg-background text-foreground overflow-hidden">
      {/* Sidebar */}
      <div className="w-[280px] flex flex-col bg-secondary border-r border-border">
        <TitleBar />
        
        <ScrollArea className="flex-1">
          <div className="p-3 space-y-4">
            {/* Threads Section */}
            <div className="space-y-2">
              <div className="flex items-center justify-between px-2">
                <h2 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider">Threads</h2>
                <div className="flex gap-1">
                  <Tooltip>
                    <TooltipTrigger>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6"
                        onClick={() => refreshSnapshot()}
                      >
                        <RefreshCw className="h-3.5 w-3.5" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>Refresh</TooltipContent>
                  </Tooltip>
                  
                  <DropdownMenu>
                    <Tooltip>
                      <TooltipTrigger>
                        <DropdownMenuTrigger>
                          <Button variant="ghost" size="icon" className="h-6 w-6">
                            <Plus className="h-3.5 w-3.5" />
                          </Button>
                        </DropdownMenuTrigger>
                      </TooltipTrigger>
                      <TooltipContent>New Thread</TooltipContent>
                    </Tooltip>
                    <DropdownMenuContent align="start" className="w-56">
                      {(bootstrap?.providers ?? []).map((p) => (
                        <DropdownMenuSub key={p.kind}>
                          <DropdownMenuSubTrigger className="gap-2">
                            <div className={`w-2 h-2 rounded-full ${PROVIDER_COLORS[p.kind]}`} />
                            New {PROVIDER_LABELS[p.kind]} Thread
                          </DropdownMenuSubTrigger>
                          <DropdownMenuSubContent>
                            {p.models.map(({ value, label }) => (
                              <DropdownMenuItem
                                key={value}
                                onClick={() => startSession(p.kind, value)}
                                className="gap-2"
                              >
                                {label}
                              </DropdownMenuItem>
                            ))}
                            {p.models.length > 0 && <DropdownMenuSeparator />}
                            <DropdownMenuItem
                              onClick={() => {
                                send({ type: "refresh_models", provider: p.kind });
                                setLastAction(
                                  `Refreshing ${PROVIDER_LABELS[p.kind]} models...`
                                );
                              }}
                              className="gap-2 text-muted-foreground"
                            >
                              <RefreshCw className="h-3.5 w-3.5" />
                              Refresh models
                            </DropdownMenuItem>
                          </DropdownMenuSubContent>
                        </DropdownMenuSub>
                      ))}
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>
              </div>

              <div className="space-y-1">
                {snapshot.sessions.length === 0 ? (
                  <div className="px-2 py-4 text-sm text-muted-foreground text-center">
                    No threads yet
                  </div>
                ) : (
                  snapshot.sessions.map((session) => (
                    <ThreadItem
                      key={session.summary.sessionId}
                      session={session}
                      isActive={session.summary.sessionId === activeSessionId}
                      onClick={() => setActiveSessionId(session.summary.sessionId)}
                      onDelete={() => deleteSession(session.summary.sessionId)}
                    />
                  ))
                )}
              </div>
            </div>

            <Separator />

            {/* Providers Section */}
            <div className="space-y-2">
              <h2 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider px-2">Providers</h2>
              <div className="space-y-1">
                {bootstrap?.providers.map((provider) => (
                  <ProviderStatusItem key={provider.kind} status={provider} />
                ))}
              </div>
            </div>
          </div>
        </ScrollArea>

        {/* Status Bar */}
        <div className="h-7 px-3 flex items-center justify-between text-[11px] text-muted-foreground border-t border-border bg-secondary">
          <div className="flex items-center gap-2">
            <div className={`w-1.5 h-1.5 rounded-full ${
              connectionStatus === "connected" ? "bg-green-500" :
              connectionStatus === "connecting" ? "bg-yellow-500" : "bg-red-500"
            }`} />
            <span className="capitalize">{connectionStatus}</span>
            <span className="text-border">|</span>
            <span className="text-foreground/70">{lastAction}</span>
          </div>
          <div className="flex items-center gap-3">
            <span>{snapshot.sessions.length} threads</span>
          </div>
        </div>
      </div>

      {/* Main Content */}
      <div className="flex-1 flex flex-col bg-background">
        {!activeSession ? (
          <div className="flex-1 flex flex-col items-center justify-center text-muted-foreground">
            <div className="w-12 h-12 rounded-xl bg-muted flex items-center justify-center mb-4">
              <MessageSquare className="w-6 h-6" />
            </div>
            <h2 className="text-lg font-medium text-foreground mb-1">No active thread</h2>
            <p className="text-sm max-w-xs text-center">
              Select a thread from the sidebar or create a new one to get started
            </p>
          </div>
        ) : (
          <>
            {/* Header */}
            <div className="h-14 border-b border-border flex items-center justify-between px-6">
              <div className="flex items-center gap-3">
                <div className={`w-2.5 h-2.5 rounded-full ${PROVIDER_COLORS[activeSession.summary.provider]}`} />
                <div>
                  <div className="flex items-center gap-2">
                    <h1 className="font-semibold">{activeSession.summary.title}</h1>
                    {activeSession.summary.model && (
                      <Badge variant="outline" className="text-[10px] h-4 px-1.5">
                        {activeSession.summary.model}
                      </Badge>
                    )}
                  </div>
                  <p className="text-xs text-muted-foreground">
                    {PROVIDER_LABELS[activeSession.summary.provider]} · {activeSession.summary.turnCount} turns
                  </p>
                </div>
              </div>
              <div className="flex items-center gap-2">
                {activeSession.summary.status === "running" && (
                  <Button
                    variant="destructive"
                    size="sm"
                    onClick={() =>
                      send({
                        type: "interrupt_turn",
                        session_id: activeSession.summary.sessionId,
                      })
                    }
                  >
                    Interrupt
                  </Button>
                )}
              </div>
            </div>

            {/* Conversation */}
            <ScrollArea className="flex-1 p-6">
              <div className="max-w-3xl mx-auto space-y-6">
                {activeSession.turns.length === 0 ? (
                  <div className="text-center text-muted-foreground py-12">
                    <Bot className="w-8 h-8 mx-auto mb-3 opacity-50" />
                    <p>Thread ready. Send your first message below.</p>
                  </div>
                ) : (
                  activeSession.turns.map((turn) => (
                    <div key={turn.turnId} className="space-y-4 animate-fade-in">
                      {/* User Message */}
                      <div className="flex gap-3">
                        <div className="w-7 h-7 rounded-full bg-primary flex items-center justify-center shrink-0 text-primary-foreground text-xs font-medium">
                          You
                        </div>
                        <div className="flex-1 pt-0.5">
                          <p className="text-sm leading-relaxed">{turn.input}</p>
                        </div>
                      </div>

                      {/* Assistant Message */}
                      <div className="flex gap-3">
                        <div className={`w-7 h-7 rounded-full flex items-center justify-center shrink-0 text-white text-xs font-medium ${PROVIDER_COLORS[activeSession.summary.provider]}`}>
                          {PROVIDER_LABELS[activeSession.summary.provider][0]}
                        </div>
                        <div className="flex-1 pt-0.5 space-y-2">
                          <div className="flex items-center gap-2">
                            <span className="text-sm font-medium">
                              {PROVIDER_LABELS[activeSession.summary.provider]}
                            </span>
                            {turn.status === "running" && !turn.output && !turn.reasoning && !(turn.toolCalls?.length) && (
                              <Badge variant="secondary" className="text-[10px] h-4">Thinking...</Badge>
                            )}
                          </div>

                          {/* Reasoning block */}
                          {turn.reasoning && (
                            <details className="group" open={turn.status === "running"}>
                              <summary className="cursor-pointer text-xs text-muted-foreground select-none flex items-center gap-1 list-none mb-1">
                                <span className="inline-block transition-transform group-open:rotate-90">▶</span>
                                <span>Thinking</span>
                                {turn.status === "running" && (
                                  <span className="w-1.5 h-1.5 rounded-full bg-yellow-500 animate-pulse ml-1" />
                                )}
                              </summary>
                              <div className="text-xs leading-relaxed whitespace-pre-wrap text-muted-foreground bg-muted/50 rounded p-2 border-l-2 border-border">
                                {turn.reasoning}
                              </div>
                            </details>
                          )}

                          {/* Plan card */}
                          {turn.plan && (
                            <div className="rounded-md border border-border bg-muted/30 p-3 space-y-2">
                              <div className="flex items-center justify-between">
                                <div className="flex items-center gap-2">
                                  <span className="text-sm font-semibold">{turn.plan.title}</span>
                                  <Badge
                                    variant={
                                      turn.plan.status === "accepted"
                                        ? "default"
                                        : turn.plan.status === "rejected"
                                        ? "destructive"
                                        : "secondary"
                                    }
                                    className="text-[10px] h-4"
                                  >
                                    {turn.plan.status}
                                  </Badge>
                                </div>
                                {turn.plan.status === "proposed" && (
                                  <div className="flex gap-2">
                                    <Button
                                      size="sm"
                                      onClick={() =>
                                        acceptPlan(
                                          activeSession.summary.sessionId,
                                          turn.plan!.planId
                                        )
                                      }
                                    >
                                      Accept
                                    </Button>
                                    <Button
                                      size="sm"
                                      variant="outline"
                                      onClick={() =>
                                        rejectPlan(
                                          activeSession.summary.sessionId,
                                          turn.plan!.planId
                                        )
                                      }
                                    >
                                      Reject
                                    </Button>
                                  </div>
                                )}
                              </div>
                              {turn.plan.steps.length > 0 ? (
                                <ol className="list-decimal pl-5 space-y-1 text-sm">
                                  {turn.plan.steps.map((step, idx) => (
                                    <li key={idx}>
                                      {step.detail ? (
                                        <details>
                                          <summary className="cursor-pointer">
                                            {step.title}
                                          </summary>
                                          <div className="text-xs text-muted-foreground whitespace-pre-wrap mt-1">
                                            {step.detail}
                                          </div>
                                        </details>
                                      ) : (
                                        step.title
                                      )}
                                    </li>
                                  ))}
                                </ol>
                              ) : (
                                <pre className="text-xs whitespace-pre-wrap text-muted-foreground">
                                  {turn.plan.raw}
                                </pre>
                              )}
                            </div>
                          )}

                          {/* Tool calls */}
                          {(turn.toolCalls?.length ?? 0) > 0 && (
                            <div className="space-y-1">
                              {turn.toolCalls!.map((tc, i) => {
                                const fc = turn.fileChanges?.find((x) => x.callId === tc.callId);
                                const sub = turn.subagents?.find((x) => x.parentCallId === tc.callId);
                                return (
                                  <details key={tc.callId || i} className="group">
                                    <summary className="cursor-pointer text-xs select-none flex items-center gap-1.5 list-none">
                                      <span className="inline-block transition-transform group-open:rotate-90">▶</span>
                                      <span className={`w-1.5 h-1.5 rounded-full shrink-0 ${
                                        tc.status === "completed" ? "bg-green-500" :
                                        tc.status === "failed" ? "bg-red-500" : "bg-yellow-500 animate-pulse"
                                      }`} />
                                      <code className="font-mono">{tc.name}</code>
                                      {fc && (
                                        <span className="text-muted-foreground truncate max-w-[280px]">
                                          {fc.path}
                                        </span>
                                      )}
                                      <span className="text-muted-foreground ml-auto">
                                        {tc.status === "pending" ? "running…" : tc.status === "failed" ? "failed" : "done"}
                                      </span>
                                    </summary>
                                    <div className="mt-1 ml-4 space-y-1">
                                      {fc ? (
                                        <div className="text-xs font-mono">
                                          <div className="text-[10px] text-muted-foreground mb-1">
                                            {fc.operation} · {fc.path}
                                          </div>
                                          {fc.operation === "edit" && (
                                            <div className="rounded overflow-hidden border border-border">
                                              {fc.before && (
                                                <div className="bg-red-500/10 text-red-300 whitespace-pre-wrap p-1.5">
                                                  {fc.before
                                                    .split("\n")
                                                    .map((l) => `- ${l}`)
                                                    .join("\n")}
                                                </div>
                                              )}
                                              {fc.after && (
                                                <div className="bg-green-500/10 text-green-300 whitespace-pre-wrap p-1.5">
                                                  {fc.after
                                                    .split("\n")
                                                    .map((l) => `+ ${l}`)
                                                    .join("\n")}
                                                </div>
                                              )}
                                            </div>
                                          )}
                                          {fc.operation === "write" && fc.after && (
                                            <pre className="bg-green-500/10 text-green-300 whitespace-pre-wrap p-1.5 rounded border border-border">
                                              {fc.after}
                                            </pre>
                                          )}
                                          {fc.operation === "delete" && (
                                            <div className="bg-red-500/10 text-red-300 p-1.5 rounded border border-border">
                                              (deleted)
                                            </div>
                                          )}
                                        </div>
                                      ) : (
                                        <div className="text-xs text-muted-foreground bg-muted/50 rounded p-1.5 font-mono overflow-x-auto">
                                          {JSON.stringify(tc.args, null, 2)}
                                        </div>
                                      )}
                                      {tc.output && !fc && (
                                        <div className="text-xs bg-muted/30 rounded p-1.5 font-mono overflow-x-auto whitespace-pre-wrap">
                                          {tc.output}
                                        </div>
                                      )}
                                      {tc.error && (
                                        <div className="text-xs text-destructive bg-destructive/10 rounded p-1.5 font-mono">
                                          {tc.error}
                                        </div>
                                      )}
                                      {sub && (
                                        <div className="border-l-2 border-muted ml-1 pl-2 space-y-1">
                                          <div className="flex items-center gap-2">
                                            <Badge variant="outline" className="text-[10px] h-4">
                                              {sub.agentType}
                                            </Badge>
                                            <span
                                              className={`w-1.5 h-1.5 rounded-full ${
                                                sub.status === "completed"
                                                  ? "bg-green-500"
                                                  : sub.status === "failed"
                                                  ? "bg-red-500"
                                                  : "bg-yellow-500 animate-pulse"
                                              }`}
                                            />
                                            <span className="text-[10px] text-muted-foreground">
                                              {sub.status}
                                            </span>
                                          </div>
                                          <div className="text-xs text-muted-foreground italic line-clamp-2">
                                            {sub.prompt}
                                          </div>
                                          {sub.output && (
                                            <div className="text-xs bg-muted/40 rounded p-1.5 whitespace-pre-wrap">
                                              {sub.output}
                                            </div>
                                          )}
                                          {sub.error && (
                                            <div className="text-xs text-destructive bg-destructive/10 rounded p-1.5">
                                              {sub.error}
                                            </div>
                                          )}
                                        </div>
                                      )}
                                    </div>
                                  </details>
                                );
                              })}
                            </div>
                          )}

                          <div className="text-sm leading-relaxed whitespace-pre-wrap">
                            {turn.output || (turn.status === "running" ? "" : "")}
                          </div>
                        </div>
                      </div>
                    </div>
                  ))
                )}
              </div>
            </ScrollArea>

            {/* Composer */}
            <div className="p-4 border-t border-border bg-secondary">
              <div className="max-w-3xl mx-auto">
                <div className="relative">
                  <textarea
                    value={prompt}
                    onChange={(e) => setPrompt(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && !e.shiftKey) {
                        e.preventDefault();
                        sendTurn();
                      }
                    }}
                    placeholder="Ask anything..."
                    className="w-full min-h-[80px] max-h-[200px] bg-background border border-input rounded-lg p-3 pr-12 resize-none focus:outline-none focus:ring-1 focus:ring-ring"
                  />
                  <Button
                    size="icon"
                    className="absolute bottom-3 right-3 h-8 w-8"
                    disabled={!prompt.trim() || activeSession.summary.status === "running"}
                    onClick={sendTurn}
                  >
                    <Send className="h-4 w-4" />
                  </Button>
                </div>
                <div className="flex items-center justify-between mt-2 text-xs text-muted-foreground">
                  <div className="flex items-center gap-2">
                    <label htmlFor="permission-mode" className="text-xs">
                      Permissions
                    </label>
                    <select
                      id="permission-mode"
                      value={permissionMode}
                      onChange={(e) => setPermissionMode(e.target.value as PermissionMode)}
                      className="h-7 rounded-md border border-input bg-background px-2 text-xs text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
                    >
                      {(
                        ["accept_edits", "default", "plan", "bypass"] as PermissionMode[]
                      ).map((mode) => (
                        <option key={mode} value={mode}>
                          {PERMISSION_MODE_LABELS[mode]}
                        </option>
                      ))}
                    </select>
                    <span className="text-border">|</span>
                    <span>Press Enter to send, Shift+Enter for new line</span>
                  </div>
                  <span>{prompt.length} chars</span>
                </div>
              </div>
            </div>
          </>
        )}
      </div>

      {/* Permission request modal — shows oldest pending request */}
      {pendingPermissions.length > 0 && (
        <div className="absolute inset-0 flex items-center justify-center bg-background/70 backdrop-blur-sm z-50">
          <div className="w-[480px] max-w-[90vw] rounded-lg border border-border bg-card text-card-foreground shadow-xl p-5 space-y-4">
            <div className="flex items-center justify-between">
              <h3 className="text-sm font-semibold">Permission required</h3>
              {pendingPermissions.length > 1 && (
                <Badge variant="secondary" className="text-[10px]">
                  +{pendingPermissions.length - 1} queued
                </Badge>
              )}
            </div>
            <div className="space-y-2">
              <div className="text-xs text-muted-foreground">Tool</div>
              <code className="block text-xs font-mono bg-muted/50 rounded px-2 py-1">
                {pendingPermissions[0].toolName}
              </code>
            </div>
            <div className="space-y-2">
              <div className="text-xs text-muted-foreground">Input</div>
              <pre className="text-xs font-mono bg-muted/50 rounded p-2 max-h-48 overflow-auto whitespace-pre-wrap">
                {JSON.stringify(pendingPermissions[0].input, null, 2)}
              </pre>
            </div>
            <div className="grid grid-cols-2 gap-2 pt-2">
              <Button
                size="sm"
                onClick={() => answerPermission(pendingPermissions[0], "allow")}
              >
                Allow once
              </Button>
              <Button
                size="sm"
                variant="secondary"
                onClick={() => answerPermission(pendingPermissions[0], "allow_always")}
              >
                Allow always
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => answerPermission(pendingPermissions[0], "deny")}
              >
                Deny once
              </Button>
              <Button
                size="sm"
                variant="destructive"
                onClick={() => answerPermission(pendingPermissions[0], "deny_always")}
              >
                Deny always
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* User question modal */}
      {pendingQuestion && (
        <div className="absolute inset-0 flex items-center justify-center bg-background/70 backdrop-blur-sm z-50">
          <div className="w-[480px] max-w-[90vw] rounded-lg border border-border bg-card text-card-foreground shadow-xl p-5 space-y-4">
            <h3 className="text-sm font-semibold">Question from agent</h3>
            <p className="text-sm leading-relaxed whitespace-pre-wrap">
              {pendingQuestion.question}
            </p>
            <textarea
              value={questionDraft}
              onChange={(e) => setQuestionDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  submitQuestionAnswer();
                }
              }}
              placeholder="Type your answer..."
              className="w-full min-h-[80px] bg-background border border-input rounded-md p-2 text-sm resize-none focus:outline-none focus:ring-1 focus:ring-ring"
              autoFocus
            />
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => {
                  setPendingQuestion(null);
                  setQuestionDraft("");
                }}
              >
                Dismiss
              </Button>
              <Button size="sm" onClick={submitQuestionAnswer} disabled={!questionDraft.trim()}>
                Send
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}


