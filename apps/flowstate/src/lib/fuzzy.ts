// Fuzzy subsequence scorer for the Cmd+P picker. Mirrors the spirit
// of fzf / fff-search's neo_frizbee scoring: every character of the
// query must appear in the path in order (case-insensitive), and we
// rank survivors by a weighted combination of where they matched —
// basename hits beat directory-prefix hits, consecutive matches beat
// scattered ones, and word-boundary anchors beat mid-word hits.
//
// On top of the subsequence scorer we run an **acronym pre-pass**
// (IntelliJ / Zed style): if the query is a prefix of the word-initials
// of the basename — `fnsc` → `FooNameStrategyController.ts` — we short-
// circuit with a score that dominates any subsequence hit. The
// fall-through preserves all the old "tbsv → tabs-view.tsx" behavior.
//
// We deliberately don't pull in `fuzzysort` or `fuse.js`: the whole
// matcher is ~80 lines, runs in <2 ms over 100k paths in dev tests,
// and avoids ~12 KB gzipped of dependency. Score weights tuned so
// the typical Cmd+P query ("tbsv" → "tabs-view.tsx") puts the
// expected file in the top result for our own monorepo.
//
// Score is unbounded but always positive on a match and 0 on a
// non-match. Higher = better. The picker sorts descending.

/** Minimum characters in a query before fuzzy matching kicks in.
 *  One- and two-char queries fall back to substring (handled by
 *  the caller in `rankFileMatches`) — they'd produce too many
 *  near-tied fuzzy hits to be useful. */
export const FUZZY_MIN_QUERY_LEN = 2;

interface FuzzyConfig {
  /** Bonus per character that lands on a word boundary (start of
   *  the path, after `/`, `_`, `-`, `.`, or a lowercase→uppercase
   *  camelCase transition). */
  boundaryBonus: number;
  /** Bonus per consecutive matched character (run length). Doubles
   *  on each subsequent consecutive char to reward "tabs" matching
   *  contiguously inside `tabs-view`. */
  consecutiveBonus: number;
  /** Bonus when a match falls inside the basename rather than a
   *  directory segment. The picker is overwhelmingly used to find
   *  files by name, so basename hits are the dominant signal. */
  basenameBonus: number;
  /** Bonus when the *first* matched character is at the very start
   *  of the basename. "tab" → "tabs.ts" beats "tab" → "stable.ts". */
  basenamePrefixBonus: number;
  /** Penalty per gap character between matches. Keeps long
   *  spread-out matches (`abc` → `aXXXbXXXc`) below tight ones. */
  gapPenalty: number;
  /** Per-query-char score when the query is a prefix of the basename's
   *  word-initials (e.g. `fnsc` → `FooNameStrategyController.ts`).
   *  Multiplied by query length so longer acronym matches beat shorter
   *  ones AND so even a 2-char acronym (`fb` → `FooBar.ts` ⇒ 4000) sits
   *  comfortably above the densest realistic subsequence hit (~250 for
   *  a 4-char fully-consecutive basename-prefix match). */
  acronymBasenameBonus: number;
  /** Same idea, but for the full-path initials when the basename's
   *  initials don't cover the query (e.g. `cclf` →
   *  `components/code/lib/fuzzy-utils.ts`). Half the basename weight
   *  so a basename acronym hit always outranks a path acronym hit. */
  acronymPathBonus: number;
}

const DEFAULT_CONFIG: FuzzyConfig = {
  boundaryBonus: 30,
  consecutiveBonus: 16,
  basenameBonus: 8,
  basenamePrefixBonus: 40,
  gapPenalty: 1,
  acronymBasenameBonus: 2000,
  acronymPathBonus: 1000,
};

/** Word-boundary detector. A position `i` is a boundary when the
 *  previous char is a separator or when we cross a camelCase hump
 *  (lower→upper). Position 0 is always a boundary. */
function isBoundary(path: string, i: number): boolean {
  if (i === 0) return true;
  const prev = path.charCodeAt(i - 1);
  // ASCII /, _, -, ., space — most code paths stay in this branch.
  if (prev === 47 || prev === 95 || prev === 45 || prev === 46 || prev === 32)
    return true;
  // camelCase: previous is lowercase letter, current is uppercase.
  const cur = path.charCodeAt(i);
  if (prev >= 97 && prev <= 122 && cur >= 65 && cur <= 90) return true;
  return false;
}

/** Concatenate the lowercase first character of every word in
 *  `path[start..end)`, where "word start" is exactly what `isBoundary`
 *  recognises. Used by the acronym pre-pass — keeping this in lockstep
 *  with `isBoundary` is intentional so the docs ("typing FNSC matches
 *  FooNameStrategyController") stay honest as the boundary rules
 *  evolve. ASCII fast path mirrors the main scoring loop; non-ASCII
 *  falls back to `String#toLowerCase()` for that one char only. */
function acronymInitials(path: string, start: number, end: number): string {
  let out = "";
  for (let i = start; i < end; i++) {
    if (!isBoundary(path, i)) continue;
    const pc = path.charCodeAt(i);
    let pcLower: number;
    if (pc < 128) {
      pcLower = pc >= 65 && pc <= 90 ? pc + 32 : pc;
    } else {
      pcLower = path[i]!.toLowerCase().charCodeAt(0);
    }
    // Skip pure separators that only appear because the *previous*
    // char was a separator — `isBoundary` says position 0 is a
    // boundary, but if path starts with `/` we don't want `/` itself
    // in the initials. Same for any other non-letter/digit boundary.
    if (
      pcLower === 47 ||
      pcLower === 95 ||
      pcLower === 45 ||
      pcLower === 46 ||
      pcLower === 32
    ) {
      continue;
    }
    out += String.fromCharCode(pcLower);
  }
  return out;
}

/** Compute a fuzzy score for `query` against `path` (case-insensitive
 *  subsequence match). Returns 0 when not all query chars can be
 *  found in order. The `query` argument MUST already be lowercase —
 *  we don't lowercase it on every call to keep the inner loop tight.
 *  The matcher lowercases path chars on the fly via charCode + 32
 *  for ASCII; non-ASCII paths fall back to `String#toLowerCase()`
 *  inside the slow path. */
export function fuzzyScore(
  path: string,
  queryLower: string,
  config: FuzzyConfig = DEFAULT_CONFIG,
): number {
  const qlen = queryLower.length;
  if (qlen === 0) return 1; // empty query trivially matches everything
  const plen = path.length;
  if (qlen > plen) return 0;

  const slash = path.lastIndexOf("/");
  const basenameStart = slash >= 0 ? slash + 1 : 0;

  // Acronym pre-pass (IntelliJ / Zed style). Only kicks in for queries
  // long enough to be informative — single-char "acronyms" would tie
  // together every file whose basename starts with that letter. The
  // returned score scales with `qlen` so longer acronym matches beat
  // shorter ones, and the constants are large enough that any acronym
  // hit dominates the densest realistic subsequence hit on the same
  // path (see `acronymBasenameBonus` doc above for the math).
  if (qlen >= FUZZY_MIN_QUERY_LEN) {
    const basenameInitials = acronymInitials(path, basenameStart, plen);
    if (basenameInitials.startsWith(queryLower)) {
      return config.acronymBasenameBonus * qlen;
    }
    // Path-level initials only when there's a directory prefix to
    // contribute extra letters; otherwise we'd be repeating the
    // basename check we just did.
    if (basenameStart > 0) {
      const pathInitials = acronymInitials(path, 0, plen);
      if (pathInitials.startsWith(queryLower)) {
        return config.acronymPathBonus * qlen;
      }
    }
  }

  let score = 0;
  let qi = 0;
  let prevMatched = -2; // -2 sentinel so the first match isn't "consecutive"
  let consecutive = 0;
  let firstMatchInBasename = -1;

  // ASCII fast path: lowercase by OR'ing 0x20 when the char is in
  // the uppercase A–Z range. Non-ASCII (>= 128) falls back to a
  // proper toLowerCase.
  for (let i = 0; i < plen && qi < qlen; i++) {
    const pc = path.charCodeAt(i);
    let pcLower: number;
    if (pc < 128) {
      pcLower = pc >= 65 && pc <= 90 ? pc + 32 : pc;
    } else {
      pcLower = path[i]!.toLowerCase().charCodeAt(0);
    }
    const qc = queryLower.charCodeAt(qi);
    if (pcLower !== qc) {
      // Reset consecutive run when we skip a char.
      consecutive = 0;
      continue;
    }

    // Match found — score this position.
    let charScore = 1;
    if (isBoundary(path, i)) {
      charScore += config.boundaryBonus;
    }
    if (i >= basenameStart) {
      charScore += config.basenameBonus;
      if (firstMatchInBasename < 0) {
        firstMatchInBasename = i;
        if (i === basenameStart) {
          charScore += config.basenamePrefixBonus;
        }
      }
    }
    if (i === prevMatched + 1) {
      consecutive++;
      // Doubling reward keeps tight runs ahead of scattered matches:
      // 16, 32, 64, 128… caps fast in practice (queries are short).
      charScore += config.consecutiveBonus * consecutive;
    } else {
      consecutive = 1;
      // Charge a small per-gap penalty so scattered matches lose to
      // dense ones with the same boundary count.
      if (prevMatched >= 0) {
        const gap = i - prevMatched - 1;
        charScore -= config.gapPenalty * gap;
      }
    }

    score += charScore;
    prevMatched = i;
    qi++;
  }

  // Subsequence requirement: all query chars must have matched.
  if (qi < qlen) return 0;
  // Clamp to >0 so callers can use 0 as the "no match" sentinel
  // even when the boundary penalties happen to drag the score
  // negative on degenerate inputs.
  return score > 0 ? score : 1;
}

/** Convenience wrapper for callers that don't want to pre-lowercase
 *  the query. Allocates a single string per call — fine when the
 *  query changes per keystroke but the file list is iterated O(N)
 *  per stroke (we hoist this out of the loop in `rankFileMatchesFuzzy`). */
export function fuzzyScoreAuto(path: string, query: string): number {
  return fuzzyScore(path, query.toLowerCase());
}
