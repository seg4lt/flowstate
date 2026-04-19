import * as React from "react";
import { FileText } from "lucide-react";

interface MentionPopupProps {
  /** Forward-slash relative file paths (already filtered + ranked). */
  matches: string[];
  selectedIndex: number;
  onSelect: (path: string) => void;
}

/** Splits a forward-slash path into `[dirname, basename]`. Dirname is
 *  `""` when the path is a bare filename. */
function splitPath(path: string): [string, string] {
  const slash = path.lastIndexOf("/");
  if (slash < 0) return ["", path];
  return [path.slice(0, slash), path.slice(slash + 1)];
}

/** Autocomplete popup that floats above the chat textarea while the
 *  user is typing an `@<filename>` mention. Mirrors the keyboard /
 *  focus / scroll behavior of `SlashCommandPopup` so the two popups
 *  feel like siblings — the only differences are the row layout
 *  (basename + muted dirname hint) and the file icon. */
export function MentionPopup({
  matches,
  selectedIndex,
  onSelect,
}: MentionPopupProps) {
  const listRef = React.useRef<HTMLDivElement>(null);

  // Keep the selected item scrolled into view as the user arrows
  // through a long list. `block: "nearest"` avoids yanking the
  // viewport when the row is already visible.
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
      {matches.map((path, i) => {
        const [dir, base] = splitPath(path);
        return (
          <div
            key={path}
            role="option"
            aria-selected={i === selectedIndex}
            onMouseDown={(e) => {
              // mouseDown (not click) so the textarea doesn't lose
              // focus before we can route the pick back into the
              // composer.
              e.preventDefault();
              onSelect(path);
            }}
            className={`flex cursor-pointer items-center gap-2 rounded-md px-2 py-1 text-xs ${
              i === selectedIndex
                ? "bg-accent text-accent-foreground"
                : "text-popover-foreground hover:bg-accent/50"
            }`}
          >
            <FileText className="h-3 w-3 shrink-0 text-muted-foreground" />
            <span className="truncate font-mono">{base}</span>
            {dir && (
              <span className="ml-auto truncate pl-3 font-mono text-[10px] text-muted-foreground/70">
                {dir}
              </span>
            )}
          </div>
        );
      })}
      <div className="mt-1 border-t border-border/50 px-2 pt-1 text-[10px] text-muted-foreground/70">
        Enter to insert · Esc to close
      </div>
    </div>
  );
}
