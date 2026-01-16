import { useEffect, useMemo, useRef, useState } from "react";

type ProviderKind = "codex" | "claude" | "github_copilot";
type ProviderStatusLevel = "ready" | "warning" | "error";
type SessionStatus = "ready" | "running" | "interrupted";
type TurnStatus = "running" | "completed" | "interrupted" | "failed";

type ProviderStatus = {
  kind: ProviderKind;
  label: string;
  installed: boolean;
  authenticated: boolean;
  version: string | null;
  status: ProviderStatusLevel;
  message: string | null;
};

type TurnRecord = {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  createdAt: string;
  updatedAt: string;
};

type SessionSummary = {
  sessionId: string;
  provider: ProviderKind;
  title: string;
  status: SessionStatus;
  createdAt: string;
  updatedAt: string;
  lastTurnPreview: string | null;
  turnCount: number;
};

type SessionDetail = {
  summary: SessionSummary;
  turns: TurnRecord[];
};

type AppSnapshot = {
  generatedAt: string;
  sessions: SessionDetail[];
};

type BootstrapPayload = {
  appName: string;
  generatedAt: string;
  wsUrl: string;
  providers: ProviderStatus[];
  snapshot: AppSnapshot;
};

type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; sessionId: string; turn: TurnRecord }
  | {
      type: "content_delta";
      sessionId: string;
      turnId: string;
      delta: string;
      accumulatedOutput: string;
    }
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

type ActivityEntry = {
  id: string;
  title: string;
  body: string;
  at: number;
};

const EMPTY_SNAPSHOT: AppSnapshot = {
  generatedAt: new Date(0).toISOString(),
  sessions: [],
};

function formatProvider(kind: ProviderKind) {
  switch (kind) {
    case "codex": return "Codex";
    case "claude": return "Claude";
    case "github_copilot": return "GitHub Copilot";
  }
}

function formatRelative(timestamp: string) {
  const value = new Date(timestamp);
  if (Number.isNaN(value.getTime())) return timestamp;

  const seconds = Math.floor((Date.now() - value.getTime()) / 1000);
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

function activityId() {
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export default function App() {
  const [bootstrap, setBootstrap] = useState<BootstrapPayload | null>(null);
  const [snapshot, setSnapshot] = useState<AppSnapshot>(EMPTY_SNAPSHOT);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [transportState, setTransportState] = useState("Booting local runtime...");
  const [activity, setActivity] = useState<ActivityEntry[]>([]);
  const socketRef = useRef<WebSocket | null>(null);
  const conversationRef = useRef<HTMLDivElement | null>(null);

  const activeSession = useMemo(
    () => snapshot.sessions.find((session) => session.summary.sessionId === activeSessionId) ?? null,
    [activeSessionId, snapshot.sessions],
  );

  function pushActivity(title: string, body = "") {
    setActivity((current) => [{ id: activityId(), title, body, at: Date.now() }, ...current].slice(0, 6));
  }

  function applySnapshot(nextSnapshot: AppSnapshot, preferredSessionId?: string | null) {
    setSnapshot(nextSnapshot);
    setActiveSessionId((current) => {
      const requested = preferredSessionId ?? current;
      if (!nextSnapshot.sessions.length) return null;
      if (requested && nextSnapshot.sessions.some((session) => session.summary.sessionId === requested)) {
        return requested;
      }
      return nextSnapshot.sessions[0].summary.sessionId;
    });
  }

  async function loadSnapshot() {
    const response = await fetch("/api/snapshot");
    if (!response.ok) {
      throw new Error(`Snapshot failed with ${response.status}`);
    }
    return (await response.json()) as AppSnapshot;
  }

  async function refreshSnapshot(preferredSessionId?: string | null) {
    try {
      const nextSnapshot = await loadSnapshot();
      applySnapshot(nextSnapshot, preferredSessionId);
    } catch (error) {
      pushActivity("Snapshot error", error instanceof Error ? error.message : String(error));
    }
  }

  function send(message: unknown) {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      pushActivity("Transport closed", "The websocket is not connected yet.");
      return;
    }

    socket.send(JSON.stringify(message));
  }

  function handleRuntimeEvent(event: RuntimeEvent) {
    switch (event.type) {
      case "runtime_ready":
        pushActivity("Runtime ready", event.message);
        break;
      case "session_started":
        pushActivity("Thread started", event.session.title);
        void refreshSnapshot(event.session.sessionId);
        break;
      case "turn_started":
        pushActivity("Turn started", event.turn.input);
        break;
      case "content_delta":
        pushActivity("Response received", event.delta.slice(0, 160));
        break;
      case "turn_completed":
        pushActivity("Turn completed", event.turn.output.slice(0, 160));
        void refreshSnapshot(event.session.sessionId);
        break;
      case "session_interrupted":
        pushActivity("Thread interrupted", event.message);
        void refreshSnapshot(event.session.sessionId);
        break;
      case "error":
        pushActivity("Runtime error", event.message);
        break;
      case "info":
        pushActivity("Info", event.message);
        break;
    }
  }

  useEffect(() => {
    let disposed = false;

    async function boot() {
      try {
        const response = await fetch("/api/bootstrap");
        if (!response.ok) {
          throw new Error(`Bootstrap failed with ${response.status}`);
        }

        const nextBootstrap = (await response.json()) as BootstrapPayload;
        if (disposed) return;

        setBootstrap(nextBootstrap);
        applySnapshot(nextBootstrap.snapshot);
        setTransportState("Bootstrapped");
        pushActivity("Bootstrap loaded", `Loaded ${nextBootstrap.appName}.`);

        const socket = new WebSocket(nextBootstrap.wsUrl);
        socketRef.current = socket;

        socket.addEventListener("open", () => {
          if (disposed) return;
          setTransportState("Connected");
          pushActivity("Connected", "Local websocket runtime is online.");
        });

        socket.addEventListener("close", () => {
          if (disposed) return;
          setTransportState("Disconnected");
          pushActivity("Disconnected", "Websocket connection closed.");
        });

        socket.addEventListener("message", (rawEvent) => {
          if (disposed) return;

          const payload = JSON.parse(rawEvent.data as string) as ServerMessage;
          switch (payload.type) {
            case "welcome":
              setBootstrap(payload.bootstrap);
              applySnapshot(payload.bootstrap.snapshot, payload.bootstrap.snapshot.sessions[0]?.summary.sessionId ?? null);
              break;
            case "snapshot":
              applySnapshot(payload.snapshot);
              break;
            case "session_created":
              void refreshSnapshot(payload.session.sessionId);
              break;
            case "ack":
              pushActivity("Ack", payload.message);
              break;
            case "error":
              pushActivity("Error", payload.message);
              break;
            case "event":
              handleRuntimeEvent(payload.event);
              break;
            case "pong":
              pushActivity("Pong", "Server heartbeat received.");
              break;
          }
        });
      } catch (error) {
        if (disposed) return;
        setTransportState("Bootstrap failed");
        pushActivity("Bootstrap failed", error instanceof Error ? error.message : String(error));
      }
    }

    void boot();

    return () => {
      disposed = true;
      socketRef.current?.close();
      socketRef.current = null;
    };
  }, []);

  useEffect(() => {
    if (!conversationRef.current) return;
    conversationRef.current.scrollTop = conversationRef.current.scrollHeight;
  }, [activeSession, snapshot]);

  const composerDisabled = !activeSession || prompt.trim().length === 0;

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="traffic">
            <span />
            <span />
            <span />
          </div>
          <h1>ZenUI</h1>
          <span className="badge">Alpha</span>
        </div>

        <section className="sidebar-section">
          <h2 className="section-label">
            <span>Threads</span>
            <button type="button" onClick={() => send({ type: "load_snapshot" })}>
              Refresh
            </button>
          </h2>
          <div className="sidebar-actions">
            <button
              className="primary"
              type="button"
              onClick={() => send({ type: "start_session", provider: "codex", title: null })}
            >
              New Codex
            </button>
            <button
              type="button"
              onClick={() => send({ type: "start_session", provider: "claude", title: null })}
            >
              New Claude
            </button>
            <button
              type="button"
              onClick={() => send({ type: "start_session", provider: "github_copilot", title: null })}
            >
              New Copilot
            </button>
          </div>
        </section>

        <section className="sidebar-section sessions-panel">
          <div className="session-list">
            {snapshot.sessions.length === 0 ? (
              <p className="muted">No threads yet. Start one above.</p>
            ) : (
              snapshot.sessions.map((session) => (
                <button
                  key={session.summary.sessionId}
                  type="button"
                  className={`session-item${session.summary.sessionId === activeSessionId ? " active" : ""}`}
                  onClick={() => setActiveSessionId(session.summary.sessionId)}
                >
                  <header>
                    <strong>{session.summary.title}</strong>
                    <span className={`status-text status-${session.summary.status}`}>
                      {session.summary.status}
                    </span>
                  </header>
                  <p>
                    {formatProvider(session.summary.provider)} · {session.summary.turnCount} turns
                  </p>
                  <p className="muted">{session.summary.lastTurnPreview ?? "No messages yet."}</p>
                </button>
              ))
            )}
          </div>
        </section>

        <section className="sidebar-section">
          <h2 className="section-label">Providers</h2>
          <div className="provider-list">
            {(bootstrap?.providers ?? []).map((provider) => (
              <article key={provider.kind} className={`provider-chip status-${provider.status}`}>
                <header>
                  <div className="status-block">
                    <span className="provider-dot" />
                    <strong>{provider.label}</strong>
                  </div>
                  <span>{provider.status}</span>
                </header>
                <p>
                  {[
                    provider.installed ? "installed" : "missing",
                    provider.authenticated ? "authenticated" : "not authenticated",
                    provider.version,
                  ]
                    .filter(Boolean)
                    .join(" · ")}
                </p>
                <p className="muted">{provider.message ?? "No provider details."}</p>
              </article>
            ))}
          </div>
        </section>

        <section className="sidebar-section">
          <h2 className="section-label">Activity</h2>
          <div className="activity-list">
            {activity.length === 0 ? (
              <p className="muted">No runtime events yet.</p>
            ) : (
              activity.map((entry) => (
                <article key={entry.id} className="activity-item">
                  <header>
                    <strong>{entry.title}</strong>
                    <span className="muted">{new Date(entry.at).toLocaleTimeString()}</span>
                  </header>
                  <p>{entry.body}</p>
                </article>
              ))
            )}
          </div>
        </section>

        <div className="sidebar-footer">
          <span>{transportState}</span>
          <span>Local</span>
        </div>
      </aside>

      <main className="main">
        <header className="toolbar">
          <div className="toolbar-row">
            <div className="toolbar-title">
              <h2>{activeSession?.summary.title ?? "Select a thread"}</h2>
              <p>
                {activeSession?.summary.lastTurnPreview ??
                  "Start a Codex or Claude thread from the left sidebar."}
              </p>
            </div>

            <div className="toolbar-actions">
              <button
                type="button"
                disabled={!activeSession}
                onClick={() => {
                  if (!activeSession) return;
                  send({ type: "interrupt_turn", session_id: activeSession.summary.sessionId });
                }}
              >
                Interrupt
              </button>
            </div>
          </div>

          <div className="toolbar-row">
            <div className="toolbar-pills">
              <span className="pill">{activeSession ? formatProvider(activeSession.summary.provider) : "No provider"}</span>
              <span className={`pill status-${activeSession?.summary.status ?? "ready"}`}>
                {activeSession?.summary.status ?? "idle"}
              </span>
              <span className="pill">{activeSession?.summary.turnCount ?? 0} turns</span>
            </div>

            <div className="toolbar-pills">
              <span className="pill mono">{activeSession?.summary.sessionId ?? "No session"}</span>
              <span className="pill">
                {activeSession ? `Updated ${formatRelative(activeSession.summary.updatedAt)}` : "Not started"}
              </span>
            </div>
          </div>
        </header>

        <section ref={conversationRef} className="conversation">
          <div className="messages">
            {!activeSession ? (
              <div className="empty-state">
                <div>
                  <strong>No active thread</strong>
                  <p>Start a Codex or Claude thread from the left, then continue the conversation here.</p>
                </div>
              </div>
            ) : activeSession.turns.length === 0 ? (
              <div className="message system">
                <div className="message-label">Session Ready</div>
                <div className="bubble">
                  Thread created for {formatProvider(activeSession.summary.provider)}. Send the first prompt below.
                </div>
              </div>
            ) : (
              activeSession.turns.flatMap((turn) => [
                <div key={`${turn.turnId}-user`} className="message user">
                  <div className="message-label">You</div>
                  <div className="bubble">{turn.input}</div>
                </div>,
                <div key={`${turn.turnId}-assistant`} className="message assistant">
                  <div className="message-meta">
                    <div className="message-label">{formatProvider(activeSession.summary.provider)}</div>
                    <span className={`status-text status-${turn.status}`}>{turn.status}</span>
                  </div>
                  <div className="bubble">{turn.output || "Awaiting response..."}</div>
                </div>,
              ])
            )}
          </div>
        </section>

        <footer className="composer-shell">
          <div className="composer">
            <textarea
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              placeholder="Ask anything, continue the current thread, or start a new one from the sidebar."
            />

            <div className="composer-meta">
              <div className="composer-left">
                <span className="pill">{activeSession ? formatProvider(activeSession.summary.provider) : "No active provider"}</span>
                <span className="pill">Local runtime</span>
                <span className={`pill status-${activeSession?.summary.status ?? "ready"}`}>
                  {activeSession?.summary.status ?? "ready"}
                </span>
              </div>

              <div className="composer-right">
                <div className="token-ring">{prompt.trim().length}</div>
                <button
                  className="primary"
                  type="button"
                  disabled={composerDisabled}
                  onClick={() => {
                    if (!activeSession || prompt.trim().length === 0) return;
                    send({
                      type: "send_turn",
                      session_id: activeSession.summary.sessionId,
                      input: prompt,
                    });
                    setPrompt("");
                  }}
                >
                  Send
                </button>
              </div>
            </div>
          </div>
        </footer>
      </main>
    </div>
  );
}
