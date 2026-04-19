// Root store composition point. Re-exports the current single-reducer
// provider (`AppProvider`) under the name `RootStoreProvider` and the
// narrow slice hooks published by the four slice modules. The
// individual slice files remain the canonical import path for
// components — a component that only reads session state should still
// import `useSessionSlice` from `stores/slices/session-slice.ts`, not
// from here. This file exists so the composition of the store is
// named explicitly in one place, matching the Phase 5 audit's
// "compose slices" step even while the underlying reducer remains in
// `app-store.tsx`.

export { AppProvider as RootStoreProvider, useApp } from "./app-store";
export { useSessionSlice } from "./slices/session-slice";
export { usePendingSlice } from "./slices/pending-slice";
export { useProviderSlice } from "./slices/provider-slice";
export { useProjectSlice } from "./slices/project-slice";
