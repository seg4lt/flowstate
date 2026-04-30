import { fuzzyScore } from "./fuzzy";

// Pure helpers for `@<filename>` mention autocomplete in the chat
// composer. Keeping them as standalone functions (no React, no I/O)
// makes the trigger / ranking / caret math unit-testable in isolation.
//
// Contract summary:
// - The caller tracks the textarea's current value and caret offset.
// - `detectMentionContext` says whether the caret sits inside a
//   mention token and, if so, returns the `@` index and the partial
//   query after it.
// - `rankFileMatches` filters a project-file list against that
//   partial query with a light basename / prefix / substring ranking.
// - `applyMentionPick` rewrites the value so the chosen path replaces
//   the partial token and returns the new caret position.

/** Context describing an in-progress `@` mention at the caret.
 *  - `atIndex`: absolute offset of the `@` character in `value`.
 *  - `query`: the partial path typed after `@` (may be empty). */
export interface MentionContext {
  atIndex: number;
  query: string;
}

/** Inspect `value` at `caret` and return the current mention context
 *  if the caret sits inside a `@<word>` token.
 *
 *  Trigger rule (per design): `@` triggers the picker ANYWHERE in the
 *  value. The only requirement is that the `@` starts a non-whitespace
 *  token and every character between `@` and the caret is also
 *  non-whitespace. The caller decides whether to show the popup — this
 *  function only detects the lexical context.
 *
 *  Returns null when:
 *   - the caret is at or past the first whitespace after the token,
 *   - the token the caret is inside does not start with `@`,
 *   - `caret` is 0. */
export function detectMentionContext(
  value: string,
  caret: number,
): MentionContext | null {
  if (caret <= 0 || caret > value.length) return null;

  // Walk left from caret-1 while the char is non-whitespace AND
  // non-`@`. The walk stops at either a whitespace boundary (no
  // mention here) or the `@` itself (mention candidate). Stopping
  // at `@` — not only at whitespace — is what lets `foo@bar`
  // trigger on `bar` per the "trigger anywhere" design decision.
  let start = caret - 1;
  while (start >= 0) {
    const ch = value[start]!;
    if (/\s/.test(ch)) return null;
    if (ch === "@") break;
    start--;
  }
  if (start < 0 || value[start] !== "@") return null;

  const query = value.slice(start + 1, caret);
  // Defensive: the walk already guarantees no whitespace in the
  // query, but keep the check so future edits can't regress this
  // invariant silently.
  if (/\s/.test(query)) return null;

  return { atIndex: start, query };
}

/** Default cap for the `@mention` autocomplete popup. The Cmd+P
 *  picker passes its own (larger) cap — see `code-view.tsx` — so the
 *  bigger virtualised list isn't constrained by the mention popup's
 *  desire to stay scannable. */
const MAX_RESULTS = 50;

interface Scored {
  path: string;
  score: number;
  // Tiebreaker: lower is earlier in the original input order.
  index: number;
}

/** Ranking mode for {@link rankFileMatches}.
 *
 *  - `"substring"` (default): the original 5-tier scorer. Fast, no
 *    typo tolerance — every char of `query` must appear contiguously.
 *    What the chat composer's `@mention` popup wants since users
 *    typing `@hand` expect "handler.ts", not "h-and.ts".
 *  - `"fuzzy"`: subsequence matcher from `lib/fuzzy.ts`. Each query
 *    char must appear in order but gaps are allowed; survivors are
 *    weighted by basename hits, word boundaries, and consecutive
 *    runs. Drives the Cmd+P picker's "Fz" toggle. */
export type RankMode = "substring" | "fuzzy";

/** Filter + lightly rank `files` against `query`. Matching is
 *  case-insensitive. An empty query returns the first `limit`
 *  entries in the input's order (alphabetical, per
 *  `listProjectFiles`) — no scoring work for the no-input case.
 *
 *  In `"substring"` mode (default), scoring is (lower = better):
 *    0  basename exact match
 *    1  basename starts with query
 *    2  any segment boundary starts with query (path-prefix)
 *    3  basename contains query
 *    4  full path contains query
 *  Non-matches are dropped.
 *
 *  In `"fuzzy"` mode, scoring delegates to `fuzzyScore` from
 *  `lib/fuzzy.ts` — subsequence match required, ranked by basename /
 *  boundary / consecutive-run bonuses (higher = better, mapped into
 *  the same "lower = better" comparator via negation so we share the
 *  result-shape).
 *
 *  Pass `limit = Infinity` to keep every match (the caller is
 *  responsible for slicing). The Cmd+P picker uses this so its
 *  "+N more — refine query" header can show a true overflow count
 *  rather than capping at 50 first. */
export function rankFileMatches(
  files: readonly string[],
  query: string,
  limit: number = MAX_RESULTS,
  mode: RankMode = "substring",
): string[] {
  if (query.length === 0) {
    return limit === Infinity ? files.slice() : files.slice(0, limit);
  }

  if (mode === "fuzzy") {
    return rankFileMatchesFuzzy(files, query, limit);
  }

  const q = query.toLowerCase();
  const scored: Scored[] = [];

  for (let i = 0; i < files.length; i++) {
    const path = files[i]!;
    const lower = path.toLowerCase();
    const slash = lower.lastIndexOf("/");
    const base = slash >= 0 ? lower.slice(slash + 1) : lower;

    let score = -1;
    if (base === q) score = 0;
    else if (base.startsWith(q)) score = 1;
    else if (lower === q || lower.startsWith(q) || lower.includes(`/${q}`))
      score = 2;
    else if (base.includes(q)) score = 3;
    else if (lower.includes(q)) score = 4;

    if (score >= 0) {
      scored.push({ path, score, index: i });
    }
  }

  scored.sort((a, b) =>
    a.score !== b.score ? a.score - b.score : a.index - b.index,
  );
  const limited = limit === Infinity ? scored : scored.slice(0, limit);
  return limited.map((s) => s.path);
}

/** Fuzzy-mode helper. Hoisted into a separate function to keep the
 *  hot substring path tight (no branch on `mode` per file). Lowercases
 *  the query exactly once before iterating; `fuzzyScore` itself
 *  lowercases path chars on the fly via the ASCII charCode trick so
 *  we don't allocate a lowercased copy of every path. */
function rankFileMatchesFuzzy(
  files: readonly string[],
  query: string,
  limit: number,
): string[] {
  const queryLower = query.toLowerCase();
  // Higher score = better in fuzzy mode; we mirror substring mode's
  // "lower = better" comparator by negating so the sort logic below
  // (and any future merging) doesn't have to fork on direction.
  const scored: Scored[] = [];
  for (let i = 0; i < files.length; i++) {
    const path = files[i]!;
    const s = fuzzyScore(path, queryLower);
    if (s > 0) {
      scored.push({ path, score: -s, index: i });
    }
  }

  scored.sort((a, b) =>
    a.score !== b.score ? a.score - b.score : a.index - b.index,
  );
  const limited = limit === Infinity ? scored : scored.slice(0, limit);
  return limited.map((s) => s.path);
}

/** Replace the partial mention token at `[atIndex, caret)` with
 *  `@<picked> ` (trailing space) and report the new caret position
 *  (one past the inserted space). Surrounding text is preserved.
 *
 *  If the char currently at `caret` in the original value is already
 *  whitespace we skip inserting our own space — avoids doubling up
 *  when the user backs up into an existing token. */
export function applyMentionPick(
  value: string,
  atIndex: number,
  caret: number,
  picked: string,
): { value: string; caret: number } {
  const before = value.slice(0, atIndex);
  const after = value.slice(caret);
  const nextCharIsSpace = after.length > 0 && /\s/.test(after[0]!);
  const insertion = nextCharIsSpace ? `@${picked}` : `@${picked} `;
  const nextValue = before + insertion + after;
  const nextCaret = before.length + insertion.length;
  return { value: nextValue, caret: nextCaret };
}
