import * as React from "react";
import { Search } from "lucide-react";
import {
  DropdownMenuItem,
  DropdownMenuSubContent,
} from "@/components/ui/dropdown-menu";
import type { ProviderModel } from "@/lib/types";

/**
 * Compact "Free" pill shown next to models the provider bills at
 * zero cost (opencode surfaces these via the `cost` object on its
 * catalog). Kept deliberately small and low-contrast so long lists
 * still read as a single column, not a decoration-heavy grid.
 */
function FreeBadge() {
  return (
    <span className="ml-auto shrink-0 rounded-sm border border-emerald-500/30 bg-emerald-500/10 px-1 py-px text-[9px] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
      Free
    </span>
  );
}

interface SearchableModelSubMenuProps {
  /** Models to render, in display order. */
  models: ProviderModel[];
  /** Called with `model.value` when the user picks a row. */
  onSelect: (modelValue: string) => void;
}

// Above this many entries the picker grows a search box at the top.
// Small catalogs (Claude's handful of canonical models) stay compact;
// large ones (opencode flattens every provider × model pair, easily 30+
// entries) get filterable.
const SEARCH_THRESHOLD = 8;

// Hard ceiling on the scrollable list's height so long catalogs never
// overflow the viewport. Matches the in-session model picker's max so
// both popups feel consistent. `DropdownMenuSubContent` itself ships
// with `overflow-hidden` and no max-height, which is why we need to
// cap + overflow an inner wrapper.
const LIST_MAX_HEIGHT_CLASS = "max-h-72";

/**
 * Scrollable, optionally-searchable variant of a model submenu for the
 * sidebar's "New <provider> thread" dropdowns.
 *
 * Why this exists: Radix's `DropdownMenuSubContent` renders with
 * `overflow-hidden` and inherits no max-height from the parent menu,
 * so a long `info.models` list just extends off-screen. Wrapping the
 * items in a capped-height scroll container fixes scroll; adding an
 * `<input>` on top and filtering client-side fixes discoverability.
 *
 * The filter input is a plain `<input>` rather than a
 * `DropdownMenuItem` because Radix treats menu items as the
 * (mutually-exclusive) focusable targets of the menu's type-ahead
 * engine. Using a plain input lets us intercept keys ourselves —
 * typing mutates the filter, ArrowDown hops into the filtered list,
 * Escape bubbles up to close the menu.
 */
export function SearchableModelSubMenu({
  models,
  onSelect,
}: SearchableModelSubMenuProps) {
  const [query, setQuery] = React.useState("");
  const inputRef = React.useRef<HTMLInputElement>(null);
  const listRef = React.useRef<HTMLDivElement>(null);
  const showSearch = models.length >= SEARCH_THRESHOLD;

  const filtered = React.useMemo(() => {
    if (!query.trim()) return models;
    const needle = query.trim().toLowerCase();
    return models.filter((m) => {
      // Match against the visible label and the underlying slug —
      // opencode's slugs (`openai/gpt-5`) are what users with prior
      // exposure to the upstream API will type, while the label
      // (`"GPT-5 · OpenAI"`) is what they see in the menu.
      return (
        m.label.toLowerCase().includes(needle) ||
        m.value.toLowerCase().includes(needle)
      );
    });
  }, [models, query]);

  // Autofocus the search input when the submenu first opens.
  // DropdownMenuSub only mounts this content when expanded, so a
  // simple mount-time focus is correct — no need to subscribe to
  // `open` state.
  React.useEffect(() => {
    if (!showSearch) return;
    inputRef.current?.focus();
  }, [showSearch]);

  function handleInputKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    // Escape: let it bubble so Radix closes the (sub)menu.
    if (e.key === "Escape") return;

    // ArrowDown: hand focus to the first rendered item so the user
    // can drive the list with arrow keys after filtering. We find
    // the first `[role=menuitem]` inside our list wrapper and
    // .focus() it; Radix's menu controller then owns navigation from
    // there.
    if (e.key === "ArrowDown") {
      const firstItem = listRef.current?.querySelector<HTMLElement>(
        "[role='menuitem']:not([data-disabled])",
      );
      if (firstItem) {
        e.preventDefault();
        firstItem.focus();
      }
      return;
    }

    // Radix DropdownMenu ships with type-ahead: letter keys jump
    // focus to matching menu items. That fights with us using the
    // same keys to type into the search box, so stop typing-style
    // events from bubbling. Control/meta combos (copy-paste) should
    // still bubble so OS shortcuts keep working.
    if (!e.ctrlKey && !e.metaKey && !e.altKey) {
      e.stopPropagation();
    }
  }

  return (
    <DropdownMenuSubContent className="w-64 p-0">
      {showSearch ? (
        <div className="flex items-center gap-2 border-b border-border/60 px-2 py-1.5">
          <Search className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleInputKeyDown}
            placeholder="Search models…"
            className="w-full bg-transparent text-sm outline-hidden placeholder:text-muted-foreground"
            aria-label="Search models"
          />
        </div>
      ) : null}
      <div
        ref={listRef}
        className={`${LIST_MAX_HEIGHT_CLASS} overflow-y-auto p-1`}
      >
        {filtered.length === 0 ? (
          <div className="px-2 py-6 text-center text-xs text-muted-foreground">
            No models match.
          </div>
        ) : (
          filtered.map((m) => (
            <DropdownMenuItem
              key={m.value}
              onClick={() => onSelect(m.value)}
              className="flex items-center gap-2"
            >
              <span className="flex-1 truncate">{m.label}</span>
              {m.isFree ? <FreeBadge /> : null}
            </DropdownMenuItem>
          ))
        )}
      </div>
    </DropdownMenuSubContent>
  );
}
