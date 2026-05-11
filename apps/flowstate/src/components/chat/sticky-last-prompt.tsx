import { CornerLeftUp } from "lucide-react";

interface StickyLastPromptProps {
  /** Full text of the most recent prompt. Surfaced via the native
   *  tooltip (`title`) so the user can preview before jumping. */
  text: string;
  /** Fired when the user clicks — chat-view forwards this to
   *  `MessageList.scrollToTurn(turnId)`. */
  onJump: () => void;
}

/** Ambient icon-only affordance pinned to the top-right of the chat
 *  column. Nearly invisible at rest (faint glyph, no background, no
 *  border) so it never competes with the conversation; lights up on
 *  hover / focus and reveals the full prompt via the native tooltip.
 *
 *  Why this design:
 *  - Icon-only + tooltip means the strip is never "another message"
 *    visually — it's clearly UI chrome.
 *  - `CornerLeftUp` reads as "back / previous" — semantically the
 *    last thing you said.
 *  - Absolutely positioned so it consumes zero layout — MessageList
 *    keeps its full height, no reflow on appearance. */
export function StickyLastPrompt({ text, onJump }: StickyLastPromptProps) {
  return (
    <button
      type="button"
      onClick={onJump}
      aria-label={`Scroll to your last prompt: ${text}`}
      title={text}
      className="absolute right-3 top-3 z-20 flex h-7 w-7 items-center justify-center rounded-full text-muted-foreground/30 transition-all hover:bg-accent hover:text-foreground hover:shadow-sm focus-visible:bg-accent focus-visible:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
    >
      <CornerLeftUp className="h-3.5 w-3.5" strokeWidth={2.25} />
    </button>
  );
}
