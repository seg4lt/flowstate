// Compatibility shim. The keyboard layer now lives in
// `lib/keyboard/` with concerns split across dsl/platform/overrides/
// registry. Existing imports (`router.tsx`, `useGlobalShortcuts.ts`,
// `header-actions.tsx`, etc.) continue to work via this re-export so
// the migration was zero-churn for callers.
//
// Prefer importing from `@/lib/keyboard` in new code.

export * from "./keyboard";
