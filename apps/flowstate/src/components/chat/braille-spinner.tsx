import * as React from "react";
import { cn } from "@/lib/utils";

// Classic 10-frame braille cascade. Each frame is a U+28xx glyph
// where the raised dots cycle around the cell — the eye reads the
// motion as a small rotating block. At 12.5 fps (80ms per frame) the
// motion is smooth without being distracting.
const BRAILLE_FRAMES = [
  "⠋",
  "⠙",
  "⠹",
  "⠸",
  "⠼",
  "⠴",
  "⠦",
  "⠧",
  "⠇",
  "⠏",
];
const FRAME_MS = 80;

export type SpinnerTone = "blue" | "green" | "orange" | "neutral";

interface BrailleSpinnerProps {
  /** Colour signal. "blue" for plan mode, "orange" for bypass
   *  permissions, "green" for auto (SDK decides per tool),
   *  "neutral" for default / accept-edits (the unremarkable
   *  baseline). Callers map their own mode state onto one of these
   *  via `toneForMode()` so the spinner stays mode-agnostic. */
  tone: SpinnerTone;
  className?: string;
  /** Accessible label — announced by screen readers in place of the
   *  glyph itself. Defaults to "Loading". */
  label?: string;
}

/**
 * Tiny braille cascade spinner. Ticks a frame index via `setState`
 * on an 80ms interval and renders exactly one `<span>` — React's
 * diff on tick only touches the glyph text, not any surrounding
 * layout. Far cheaper than a CSS background-animation because the
 * text node substitution never triggers a reflow of its parent.
 *
 * Kept local to the component (own state, own timer) so mounting /
 * unmounting one doesn't affect any others and the WorkingIndicator
 * memo boundary stays stable.
 */
function BrailleSpinnerInner({
  tone,
  className,
  label = "Loading",
}: BrailleSpinnerProps) {
  const [frame, setFrame] = React.useState(0);
  React.useEffect(() => {
    const id = window.setInterval(() => {
      setFrame((n) => (n + 1) % BRAILLE_FRAMES.length);
    }, FRAME_MS);
    return () => window.clearInterval(id);
  }, []);

  const toneClass = {
    blue: "text-blue-500 dark:text-blue-400",
    green: "text-green-500 dark:text-green-400",
    orange: "text-orange-500 dark:text-orange-400",
    // Muted-foreground matches the sidebar / working-indicator text
    // baseline, so default / accept_edits spinners blend in instead
    // of signaling an anomaly.
    neutral: "text-muted-foreground",
  }[tone];

  return (
    <span
      role="status"
      aria-label={label}
      className={cn(
        "inline-block font-mono leading-none tabular-nums",
        toneClass,
        className,
      )}
    >
      {BRAILLE_FRAMES[frame]}
    </span>
  );
}

export const BrailleSpinner = React.memo(BrailleSpinnerInner);
