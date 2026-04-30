// Serialize a thread's turn history into a single markdown string for
// the "copy thread" action in the chat header. Output is verbatim:
// `input` is plain user text, `output` and `reasoning` are already
// markdown emitted by the model — we don't escape, fence, or rewrite
// them. Pasting the result into any markdown previewer renders the
// conversation exactly as the model produced it.
//
// Format (per turn):
//   ## User
//
//   {input}
//
//   ### Reasoning      ← only when reasoning is present and non-empty
//
//   {reasoning}
//
//   ## Assistant
//
//   {output}
//
// Turns are joined with a horizontal rule (`---`) so the result reads
// like a transcript. Tool calls and file changes are intentionally
// omitted: this is a paste-friendly export, not a debug dump.

import type { TurnRecord } from "@/lib/types";

export function threadToMarkdown(turns: TurnRecord[]): string {
  const sections: string[] = [];

  for (const turn of turns) {
    const input = (turn.input ?? "").trim();
    const output = (turn.output ?? "").trim();
    const reasoning = (turn.reasoning ?? "").trim();

    // Defensive: skip turns with no user text and no assistant text.
    // Reasoning alone (no input, no output) is too unusual to be worth
    // emitting on its own.
    if (!input && !output) continue;

    const parts: string[] = [];
    if (input) {
      parts.push(`## User\n\n${input}`);
    }
    if (reasoning) {
      parts.push(`### Reasoning\n\n${reasoning}`);
    }
    if (output) {
      parts.push(`## Assistant\n\n${output}`);
    }
    sections.push(parts.join("\n\n"));
  }

  return sections.join("\n\n---\n\n");
}
