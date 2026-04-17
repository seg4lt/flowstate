import * as React from "react";

interface SlashCommandPopupProps {
  matches: { name: string; description: string }[];
  selectedIndex: number;
  onSelect: (name: string) => void;
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
      className="absolute bottom-full left-0 z-50 mb-1 w-64 overflow-hidden rounded-lg border border-border bg-popover p-1 shadow-md"
    >
      {matches.map((cmd, i) => (
        <div
          key={cmd.name}
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
          <span className="font-medium">/{cmd.name}</span>
          <span className="text-xs text-muted-foreground">
            {cmd.description}
          </span>
        </div>
      ))}
    </div>
  );
}
