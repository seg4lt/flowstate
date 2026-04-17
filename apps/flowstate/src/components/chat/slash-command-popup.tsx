import * as React from "react";
import type { SlashCommandItem } from "@/lib/slash-commands";

interface SlashCommandPopupProps {
  matches: SlashCommandItem[];
  selectedIndex: number;
  onSelect: (name: string) => void;
}

/** One-word badge that labels the row's source — "project" skill,
 * "global" skill, provider "built-in", "agent", or nothing for core
 * app commands. */
function badgeFor(item: SlashCommandItem): string | null {
  if (item.kind === "user_skill") {
    return item.source === "disk_project" ? "project" : "global";
  }
  if (item.kind === "builtin") return "built-in";
  if (item.kind === "tui_only") return "tui";
  if (item.kind === "agent") return "agent";
  return null; // core app command
}

export function SlashCommandPopup({
  matches,
  selectedIndex,
  onSelect,
}: SlashCommandPopupProps) {
  const listRef = React.useRef<HTMLDivElement>(null);

  // Keep the selected item scrolled into view.
  React.useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const item = list.children[selectedIndex] as HTMLElement | undefined;
    item?.scrollIntoView({ block: "nearest" });
  }, [selectedIndex]);

  if (matches.length === 0) return null;

  return (
    <div
      ref={listRef}
      role="listbox"
      className="absolute inset-x-0 bottom-full z-50 mb-1 max-h-80 overflow-y-auto rounded-lg border border-border bg-popover p-1 shadow-md"
    >
      {matches.map((cmd, i) => {
        const badge = badgeFor(cmd);
        const prefix = cmd.kind === "agent" ? "@" : "/";
        return (
          <div
            key={cmd.id ?? cmd.name}
            role="option"
            aria-selected={i === selectedIndex}
            onMouseDown={(e) => {
              // mouseDown (not click) so the textarea doesn't lose focus
              // before we can act.
              e.preventDefault();
              onSelect(cmd.name);
            }}
            className={`flex cursor-pointer flex-col rounded-md px-2 py-1.5 text-sm ${
              i === selectedIndex
                ? "bg-accent text-accent-foreground"
                : "text-popover-foreground hover:bg-accent/50"
            }`}
          >
            <div className="flex items-baseline gap-1.5">
              <span className="truncate font-medium">
                {prefix}
                {cmd.name}
              </span>
              {cmd.argHint && (
                <span className="truncate text-xs text-muted-foreground/70">
                  {cmd.argHint}
                </span>
              )}
              {badge && (
                <span className="ml-auto shrink-0 rounded bg-muted px-1.5 py-0.5 text-[9px] font-medium uppercase tracking-wide text-muted-foreground">
                  {badge}
                </span>
              )}
            </div>
            <span className="line-clamp-2 text-xs text-muted-foreground">
              {cmd.description}
            </span>
          </div>
        );
      })}
    </div>
  );
}
