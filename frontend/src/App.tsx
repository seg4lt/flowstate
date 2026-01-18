import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "./components/ui/button";
import { Badge } from "./components/ui/badge";
import { Separator } from "./components/ui/separator";
import { ScrollArea } from "./components/ui/scroll-area";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "./components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "./components/ui/tooltip";
import { Plus, RefreshCw, Settings, X, Minus, Square, Send, MessageSquare, Bot } from "lucide-react";

type ProviderKind = "codex" | "claude" | "github_copilot";
type ProviderStatusLevel = "ready" | "warning" | "error";
type SessionStatus = "ready" | "running" | "interrupted";
type TurnStatus = "running" | "completed" | "interrupted" | "failed";

interface ProviderStatus {
  kind: ProviderKind;
  label: string;
  installed: boolean;
  authenticated: boolean;
  version: string | null;
  status: ProviderStatusLevel;
  message: string | null;
}

interface TurnRecord {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  createdAt: string;
  updatedAt: string;
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

type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; sessionId: string; turn: TurnRecord }
  | { type: "content_delta"; sessionId: string; turnId: string; delta: string; accumulatedOutput: string }
  | { type: "turn_completed"; sessionId: string; session: SessionSummary; turn: TurnRecord }
  | { type: "session_interrupted"; session: SessionSummary; message: string }
  | { type: "error"; message: string }
  | { type: "info"; message: string };

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
  return (
    <div className="flex gap-2">
      <button
        className="w-3 h-3 rounded-full bg-[#ff5f57] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => window.electron?.closeWindow?.()}
        aria-label="Close"
      >
        <X className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#febc2e] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => window.electron?.minimizeWindow?.()}
        aria-label="Minimize"
      >
        <Minus className="w-2 h-2 opacity-0 group-hover:opacity-100 text-black/60" />
      </button>
      <button
        className="w-3 h-3 rounded-full bg-[#28c840] hover:brightness-90 transition-all flex items-center justify-center group"
        onClick={() => window.electron?.maximizeWindow?.()}
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
}: {
  session: SessionDetail;
  isActive: boolean;
  onClick: () => void;
}) {
  const { summary } = session;
  const isRunning = summary.status === "running";

  return (
    <button
      onClick={onClick}
      className={`w-full flex items-center gap-2 px-2 py-1.5 rounded-md text-left transition-colors ${
        isActive
          ? "bg-accent text-accent-foreground"
          : "hover:bg-muted text-muted-foreground hover:text-foreground"
      }`}
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
    </button>
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

// Main App
export default function App() {
  const [bootstrap, setBootstrap] = useState<BootstrapPayload | null>(null);
  const [snapshot, setSnapshot] = useState<AppSnapshot>(EMPTY_SNAPSHOT);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [connectionStatus, setConnectionStatus] = useState<"connected" | "connecting" | "disconnected">("connecting");
  const socketRef = useRef<WebSocket | null>(null);

  const activeSession = useMemo(
    () => snapshot.sessions.find((s) => s.summary.sessionId === activeSessionId) ?? null,
    [activeSessionId, snapshot.sessions]
  );

  function send(message: unknown) {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify(message));
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
              refreshSnapshot(payload.session.sessionId);
              break;
            case "event":
              if (payload.event.type === "turn_completed" || payload.event.type === "session_started") {
                refreshSnapshot();
              }
              break;
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

  const startSession = (provider: ProviderKind) => {
    send({ type: "start_session", provider, title: null });
  };

  const sendTurn = () => {
    if (!activeSession || !prompt.trim()) return;
    send({
      type: "send_turn",
      session_id: activeSession.summary.sessionId,
      input: prompt,
    });
    setPrompt("");
  };

  return (
    <div className="h-screen w-screen flex bg-background text-foreground overflow-hidden">
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
                    <DropdownMenuContent align="start" className="w-48">
                      <DropdownMenuItem onClick={() => startSession("codex")} className="gap-2">
                        <div className="w-2 h-2 rounded-full bg-emerald-500" />
                        New Codex Thread
                      </DropdownMenuItem>
                      <DropdownMenuItem onClick={() => startSession("claude")} className="gap-2">
                        <div className="w-2 h-2 rounded-full bg-amber-500" />
                        New Claude Thread
                      </DropdownMenuItem>
                      <DropdownMenuItem onClick={() => startSession("github_copilot")} className="gap-2">
                        <div className="w-2 h-2 rounded-full bg-blue-500" />
                        New Copilot Thread
                      </DropdownMenuItem>
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
                  <h1 className="font-semibold">{activeSession.summary.title}</h1>
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
                            {turn.status === "running" && (
                              <Badge variant="secondary" className="text-[10px] h-4">Thinking...</Badge>
                            )}
                          </div>
                          <div className="text-sm leading-relaxed whitespace-pre-wrap">
                            {turn.output || (turn.status === "running" ? "..." : "")}
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
                  <span>Press Enter to send, Shift+Enter for new line</span>
                  <span>{prompt.length} chars</span>
                </div>
              </div>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// Add window.electron type declaration
declare global {
  interface Window {
    electron?: {
      closeWindow?: () => void;
      minimizeWindow?: () => void;
      maximizeWindow?: () => void;
    };
  }
}
