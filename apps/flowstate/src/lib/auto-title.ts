// Single source of truth for the "first 10 words of the first user
// message" auto-title rule. Used by:
//   - `chat-view.tsx::handleSend` for human-driven sends, where the
//     title appears the instant Send is clicked (no daemon round-trip).
//   - `app-store.tsx`'s `turn_started` side-effect handler for MCP-
//     spawned threads, which never pass through the composer flow.
//
// Both call sites share the same `!existingDisplay?.title` +
// `length > 0` guards, so an overlap (e.g. a human happens to send into
// the same session right as a spawn-driven first turn lands) cannot
// double-rename.

/**
 * Derive an auto-title from a turn's first user message — the first 10
 * whitespace-delimited words, joined by single spaces. Returns an empty
 * string when there's nothing to title with; callers must guard on
 * `length > 0` and skip the rename so a manually-set title isn't
 * accidentally overwritten with `""`.
 */
export function deriveAutoTitle(input: string): string {
  return input.split(/\s+/).filter(Boolean).slice(0, 10).join(" ");
}
