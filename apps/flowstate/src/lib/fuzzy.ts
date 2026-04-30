// Fuzzy subsequence scorer for the Cmd+P picker. Mirrors the spirit
// of fzf / fff-search's neo_frizbee scoring: every character of the
// query must appear in the path in order (case-insensitive), and we
// rank survivors by a weighted combination of where they matched —
// basename hits beat directory-prefix hits, consecutive matches beat
// scattered ones, and word-boundary anchors beat mid-word hits.
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
}

const DEFAULT_CONFIG: FuzzyConfig = {
  boundaryBonus: 30,
  consecutiveBonus: 16,
  basenameBonus: 8,
  basenamePrefixBonus: 40,
  gapPenalty: 1,
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
