import type { PermissionMode } from "./types";

export const MODE_ORDER: PermissionMode[] = [
  "default",
  "accept_edits",
  "plan",
  "bypass",
];

// Labels match the Claude Agent SDK's vocabulary so the UI
// terminology doesn't drift from the underlying permission mode
// names (`default` / `acceptEdits` / `plan` / `bypassPermissions`).
// Enum values (`accept_edits`, `bypass`) stay unchanged — they're
// the serde + sqlite persistence keys.
export const MODE_LABELS: Record<PermissionMode, string> = {
  default: "Default",
  accept_edits: "Accept Edits",
  plan: "Plan",
  bypass: "Bypass Permissions",
};

export function cycleMode(
  current: PermissionMode,
  direction: "forward" | "backward"
): PermissionMode {
  const currentIndex = MODE_ORDER.indexOf(current);

  if (direction === "forward") {
    const nextIndex = (currentIndex + 1) % MODE_ORDER.length;
    return MODE_ORDER[nextIndex];
  } else {
    const prevIndex =
      (currentIndex - 1 + MODE_ORDER.length) % MODE_ORDER.length;
    return MODE_ORDER[prevIndex];
  }
}
