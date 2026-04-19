import * as React from "react";
import type { ProviderKind } from "@/lib/types";

// Minimal per-view identity the MessageList / TurnView / AgentMessage
// tree needs to label the model-info popover on agent replies.
// Replaces the four-hop `providerKind` prop-drill from ChatView down
// to MessageModelInfo.
export interface SessionContextValue {
  sessionId: string;
  provider: ProviderKind | undefined;
  /** Session-level configured model. Used as the per-turn model
   *  fallback when `turn.usage.model` hasn't been populated yet
   *  (happens mid-stream and on very old rows). */
  model: string | undefined;
}

const SessionContext = React.createContext<SessionContextValue | null>(null);

export function SessionProvider({
  value,
  children,
}: {
  value: SessionContextValue;
  children: React.ReactNode;
}) {
  // Memoize by primitive fields so consumers don't re-render on
  // every ChatView render; identity changes only when the session or
  // its resolved model changes.
  const memo = React.useMemo<SessionContextValue>(
    () => ({
      sessionId: value.sessionId,
      provider: value.provider,
      model: value.model,
    }),
    [value.sessionId, value.provider, value.model],
  );
  return (
    <SessionContext.Provider value={memo}>{children}</SessionContext.Provider>
  );
}

/** Read the surrounding session's identity. Returns `null` outside a
 *  `<SessionProvider>`, so callers must tolerate absence (leaf
 *  components that live inside the chat view can safely assume a
 *  provider is set). */
export function useSessionContext(): SessionContextValue | null {
  return React.useContext(SessionContext);
}
