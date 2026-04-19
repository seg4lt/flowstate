// Session-domain slice. Covers sessions, archivedSessions, the active
// id, doneSessionIds, awaitingInputSessionIds, and the window focus
// flag — i.e. the parts of the reducer that handle `session_*` and
// `turn_*` events and the `set_active_session` / `window_focus_changed`
// actions.
//
// Today the single `appReducer` in `../app-store.tsx` still owns this
// slice's mutations. This file re-exports the narrow selector hook
// (`useSessionSlice`) so components can subscribe to just this slice
// and ignore unrelated updates. Splitting the reducer itself into
// four separate files is a mechanical follow-up; consumer code
// already reads via this export, so the eventual split is a private
// refactor inside the store.

export { useSessionSlice } from "../app-store";
