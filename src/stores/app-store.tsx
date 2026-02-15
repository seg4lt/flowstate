import * as React from "react";
import { initWebSocket, onServerMessage, sendMessage } from "@/lib/api";
import { getDaemonUrl } from "@/lib/get-daemon-url";
import type {
  ClientMessage,
  ProviderStatus,
  ProjectRecord,
  RuntimeEvent,
  ServerMessage,
  SessionSummary,
} from "@/lib/types";

interface StreamingTurn {
  turnId: string;
  accumulatedOutput: string;
  accumulatedReasoning: string;
}

interface AppState {
  providers: ProviderStatus[];
  sessions: Map<string, SessionSummary>;
  projects: ProjectRecord[];
  activeSessionId: string | null;
  streamingTurns: Map<string, StreamingTurn>;
  ready: boolean;
}

type AppAction =
  | { type: "server_message"; message: ServerMessage }
  | { type: "set_active_session"; sessionId: string | null };

function appReducer(state: AppState, action: AppAction): AppState {
  switch (action.type) {
    case "server_message":
      return handleServerMessage(state, action.message);
    case "set_active_session":
      return { ...state, activeSessionId: action.sessionId };
    default:
      return state;
  }
}

function handleServerMessage(
  state: AppState,
  message: ServerMessage,
): AppState {
  switch (message.type) {
    case "welcome": {
      const sessions = new Map<string, SessionSummary>();
      for (const detail of message.bootstrap.snapshot.sessions) {
        sessions.set(detail.summary.sessionId, detail.summary);
      }
      return {
        ...state,
        providers: message.bootstrap.providers,
        sessions,
        projects: message.bootstrap.snapshot.projects,
        ready: true,
      };
    }

    case "snapshot": {
      const sessions = new Map<string, SessionSummary>();
      for (const detail of message.snapshot.sessions) {
        sessions.set(detail.summary.sessionId, detail.summary);
      }
      return {
        ...state,
        sessions,
        projects: message.snapshot.projects,
      };
    }

    case "event":
      return handleRuntimeEvent(state, message.event);

    default:
      return state;
  }
}

function handleRuntimeEvent(state: AppState, event: RuntimeEvent): AppState {
  switch (event.type) {
    case "session_started": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      return { ...state, sessions };
    }

    case "session_deleted": {
      const sessions = new Map(state.sessions);
      sessions.delete(event.session_id);
      return {
        ...state,
        sessions,
        activeSessionId:
          state.activeSessionId === event.session_id
            ? null
            : state.activeSessionId,
      };
    }

    case "session_interrupted": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      return { ...state, sessions };
    }

    case "turn_started": {
      const streaming = new Map(state.streamingTurns);
      streaming.set(event.session_id, {
        turnId: event.turn.turnId,
        accumulatedOutput: "",
        accumulatedReasoning: "",
      });
      return { ...state, streamingTurns: streaming };
    }

    case "reasoning_delta": {
      const streaming = new Map(state.streamingTurns);
      const existing = streaming.get(event.session_id);
      streaming.set(event.session_id, {
        turnId: event.turn_id,
        accumulatedOutput: existing?.accumulatedOutput ?? "",
        accumulatedReasoning:
          (existing?.accumulatedReasoning ?? "") + event.delta,
      });
      return { ...state, streamingTurns: streaming };
    }

    case "content_delta": {
      const streaming = new Map(state.streamingTurns);
      const existing = streaming.get(event.session_id);
      streaming.set(event.session_id, {
        turnId: event.turn_id,
        accumulatedOutput: event.accumulated_output,
        accumulatedReasoning: existing?.accumulatedReasoning ?? "",
      });
      return { ...state, streamingTurns: streaming };
    }

    case "turn_completed": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      const streaming = new Map(state.streamingTurns);
      streaming.delete(event.session_id);
      return { ...state, sessions, streamingTurns: streaming };
    }

    case "project_created": {
      return { ...state, projects: [...state.projects, event.project] };
    }

    case "project_renamed": {
      return {
        ...state,
        projects: state.projects.map((p) =>
          p.projectId === event.project_id
            ? { ...p, name: event.name, updatedAt: event.updated_at }
            : p,
        ),
      };
    }

    case "project_deleted": {
      const projects = state.projects.filter(
        (p) => p.projectId !== event.project_id,
      );
      const sessions = new Map(state.sessions);
      for (const sid of event.reassigned_session_ids) {
        const s = sessions.get(sid);
        if (s) sessions.set(sid, { ...s, projectId: undefined });
      }
      return { ...state, projects, sessions };
    }

    case "session_project_assigned": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s)
        sessions.set(event.session_id, { ...s, projectId: event.project_id });
      return { ...state, sessions };
    }

    case "provider_models_updated": {
      return {
        ...state,
        providers: state.providers.map((p) =>
          p.kind === event.provider ? { ...p, models: event.models } : p,
        ),
      };
    }

    case "provider_health_updated": {
      const exists = state.providers.some((p) => p.kind === event.status.kind);
      return {
        ...state,
        providers: exists
          ? state.providers.map((p) =>
              p.kind === event.status.kind ? event.status : p,
            )
          : [...state.providers, event.status],
      };
    }

    case "session_renamed": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s) sessions.set(event.session_id, { ...s, title: event.title });
      return { ...state, sessions };
    }

    case "session_model_updated": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s) sessions.set(event.session_id, { ...s, model: event.model });
      return { ...state, sessions };
    }

    case "session_archived": {
      const sessions = new Map(state.sessions);
      sessions.delete(event.session_id);
      return {
        ...state,
        sessions,
        activeSessionId:
          state.activeSessionId === event.session_id
            ? null
            : state.activeSessionId,
      };
    }

    case "session_unarchived": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      return { ...state, sessions };
    }

    default:
      return state;
  }
}

const initialState: AppState = {
  providers: [],
  sessions: new Map(),
  projects: [],
  activeSessionId: null,
  streamingTurns: new Map(),
  ready: false,
};

interface AppContextValue {
  state: AppState;
  dispatch: React.Dispatch<AppAction>;
  send: (message: ClientMessage) => Promise<ServerMessage | null>;
}

const AppContext = React.createContext<AppContextValue | null>(null);

export function AppProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = React.useReducer(appReducer, initialState);
  const dispatchRef = React.useRef(dispatch);
  dispatchRef.current = dispatch;

  React.useEffect(() => {
    let active = true;
    let unsubscribe: (() => void) | null = null;

    getDaemonUrl().then((wsUrl) => {
      if (!active) return;
      return initWebSocket(wsUrl).then(() => {
        if (!active) return;
        unsubscribe = onServerMessage((message) => {
          if (active) {
            dispatchRef.current({ type: "server_message", message });
          }
        });
      });
    });

    return () => {
      active = false;
      unsubscribe?.();
    };
  }, []);

  const send = React.useCallback(
    (message: ClientMessage) => sendMessage(message),
    [],
  );

  const value = React.useMemo(() => ({ state, dispatch, send }), [state, send]);

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useApp() {
  const ctx = React.useContext(AppContext);
  if (!ctx) throw new Error("useApp must be used within AppProvider");
  return ctx;
}
