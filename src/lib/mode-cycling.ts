import type { PermissionMode } from "./types";

export const MODE_ORDER: PermissionMode[] = [
  "default",
  "accept_edits",
  "plan",
  "bypass",
];

export const MODE_LABELS: Record<PermissionMode, string> = {
  default: "Default",
  accept_edits: "Auto-edit",
  plan: "Plan",
  bypass: "Full access",
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
