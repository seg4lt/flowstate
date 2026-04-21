import type { SpinnerTone } from "@/components/chat/braille-spinner";
import type { PermissionMode } from "./types";

// Single source of truth for permission-mode → colour-tone mapping.
// Consumed by the working indicator, the sidebar thread spinner, and
// the composer tint so a mode change lights up every surface with the
// same hue. Green is reserved for "auto" — the mode where the SDK
// decides per tool call — so the colour signals "something active and
// opinionated is happening". Default / accept_edits fall back to the
// neutral tone: they're the unremarkable baseline and shouldn't draw
// the eye.
export function toneForMode(mode: PermissionMode | undefined): SpinnerTone {
  if (mode === "plan") return "blue";
  if (mode === "bypass") return "orange";
  if (mode === "auto") return "green";
  return "neutral";
}
