import type { PermissionMode } from "./types";

export const MODE_ORDER: PermissionMode[] = [
  "default",
  "accept_edits",
  "plan",
  "bypass",
  "auto",
];

// Labels match the Claude Agent SDK's vocabulary so the UI
// terminology doesn't drift from the underlying permission mode
// names (`default` / `acceptEdits` / `plan` / `bypassPermissions` /
// `auto`). Enum values (`accept_edits`, `bypass`) stay unchanged —
// they're the serde + sqlite persistence keys.
export const MODE_LABELS: Record<PermissionMode, string> = {
  default: "Default",
  accept_edits: "Accept Edits",
  plan: "Plan",
  bypass: "Bypass Permissions",
  auto: "Auto",
};

export function cycleMode(
  current: PermissionMode,
  direction: "forward" | "backward",
  excluded: readonly PermissionMode[] = []
): PermissionMode {
  // Skip modes the caller has gated out (e.g. `auto` for providers
  // whose adapter doesn't set `supports_auto_permission_mode`). Falls
  // back to the full order when every option is excluded, which
  // should never happen in practice.
  const candidates = MODE_ORDER.filter((mode) => !excluded.includes(mode));
  const order = candidates.length > 0 ? candidates : MODE_ORDER;
  const currentIndex = order.indexOf(current);
  // If the current mode itself is excluded (edge case during a
  // provider switch) we snap back to the start of the filtered list.
  if (currentIndex === -1) {
    return order[0];
  }

  if (direction === "forward") {
    const nextIndex = (currentIndex + 1) % order.length;
    return order[nextIndex];
  } else {
    const prevIndex = (currentIndex - 1 + order.length) % order.length;
    return order[prevIndex];
  }
}
