import type { SpinnerTone } from "@/components/chat/braille-spinner";
import type { PermissionMode } from "./types";

// Single source of truth for permission-mode → colour-tone mapping.
// Consumed by the working indicator, the sidebar thread spinner, and
// the composer tint so a mode change lights up every surface with the
// same hue. Keep the fallback as "green" — that's the unchanged
// baseline for default / accept_edits.
export function toneForMode(mode: PermissionMode | undefined): SpinnerTone {
  if (mode === "plan") return "blue";
  if (mode === "bypass") return "orange";
  return "green";
}
