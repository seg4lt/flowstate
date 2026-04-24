import { useState } from "react";
import { ChevronDown, Check, Loader2 } from "lucide-react";
import {
  Command,
  CommandEmpty,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";

interface ModelSelectorProps {
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
}

// Threshold above which the popup grows wider + shows the search
// input prominently. Below this, the list fits at a glance and the
// search box just adds visual noise — but it's still rendered for
// keyboard-first users and to keep the markup consistent.
const SEARCH_RELEVANT_MODEL_COUNT = 8;

export function ModelSelector({
  sessionId,
  provider,
  currentModel,
}: ModelSelectorProps) {
  const { state, send } = useApp();
  const [open, setOpen] = useState(false);
  const providerStatus = state.providers.find((p) => p.kind === provider);
  const models = providerStatus?.models ?? [];

  // Three empty-state branches:
  //
  // 1. Bootstrap still running (providerStatus undefined): the daemon
  //    hasn't emitted its first health snapshot yet. Show a disabled
  //    loading chip so the toolbar geometry is stable — no layout
  //    shift when models arrive.
  // 2. Provider ready but models list still empty: health() returned
  //    `models: []` and fetch_models() is in flight. Same loading
  //    chip; swaps to the real selector via the ProviderModelsUpdated
  //    broadcast a few hundred ms later.
  // 3. Provider in error / warning state (not ready): hide entirely —
  //    the real failure surface is the sidebar provider dot, not the
  //    model picker.
  if (models.length === 0) {
    const isReady = providerStatus?.status === "ready";
    const isBootstrapping = providerStatus === undefined;
    if (isReady || isBootstrapping) {
      return (
        <button
          type="button"
          disabled
          aria-busy="true"
          aria-label="Loading models"
          className="inline-flex cursor-default items-center gap-1 rounded-md px-2 py-1 text-xs text-muted-foreground"
        >
          <Loader2 className="h-3 w-3 animate-spin" />
          Loading models…
        </button>
      );
    }
    return null;
  }

  // Label resolution fallback chain:
  //   1. exact match on the session's stored model → its catalog label
  //      (the usual case once the model is resolved)
  //   2. the raw model id itself — e.g. when the SDK pinned a
  //      date-stamped variant we haven't catalogued yet
  //   3. the first entry in the provider's model list — populated from
  //      the Claude SDK bridge's `q.supportedModels()`, whose first
  //      entry IS the SDK's default, so a freshly spawned session
  //      with `session.model === undefined` shows the real default
  //      label (e.g. "Claude Opus 4.7") immediately instead of the
  //      generic "Default" placeholder that used to linger until the
  //      `model_resolved` event arrived after the first turn.
  //   4. literal "Default" — only if the provider hasn't enumerated
  //      any models yet (the `models.length === 0` branch above has
  //      already short-circuited for that case, so this is belt-and-
  //      braces).
  const currentLabel =
    models.find((m) => m.value === currentModel)?.label ??
    currentModel ??
    models[0]?.label ??
    "Default";
  const showSearch = models.length >= SEARCH_RELEVANT_MODEL_COUNT;

  async function handleSelect(model: string) {
    setOpen(false);
    await send({
      type: "update_session_model",
      session_id: sessionId,
      model,
    });
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs hover:bg-accent"
        >
          {currentLabel}
          <ChevronDown className="h-3 w-3 text-muted-foreground" />
        </button>
      </PopoverTrigger>
      {/* `p-0` and a dynamic width: small lists stay compact (min-w-56),
          catalogs above the search threshold (e.g. opencode's flattened
          provider/model list) expand to w-72 so long labels like
          `"claude-opus-4-1-20250805 · Anthropic"` don't truncate. */}
      <PopoverContent
        align="start"
        className={showSearch ? "w-80 p-0" : "min-w-56 p-0"}
      >
        <Command
          // Match on both the label the user sees and the underlying
          // value. cmdk's default filter is substring-based and
          // case-insensitive; feeding both strings in via `value`
          // makes `gpt-5` match both `"GPT-5"` and the provider/model
          // slug form `"openai/gpt-5"`.
          filter={(value, search) =>
            value.toLowerCase().includes(search.toLowerCase()) ? 1 : 0
          }
        >
          {showSearch ? (
            <CommandInput placeholder="Search models…" autoFocus />
          ) : null}
          <CommandList>
            <CommandEmpty>No models match.</CommandEmpty>
            {models.map((model) => {
              const isSelected = currentModel === model.value;
              // The string cmdk fuzzy-matches against. Join label +
              // value so searching for either works — the displayed
              // label is often pretty ("Claude Sonnet 4"), while
              // the value carries the slug ("claude-sonnet-4-0").
              const searchValue = `${model.label} ${model.value}`;
              return (
                <CommandItem
                  key={model.value}
                  value={searchValue}
                  onSelect={() => handleSelect(model.value)}
                >
                  {isSelected ? (
                    <Check className="mr-2 h-3 w-3" />
                  ) : (
                    <span className="mr-2 w-3" />
                  )}
                  <span className="flex-1 truncate">{model.label}</span>
                  {model.isFree ? (
                    <span className="ml-2 shrink-0 rounded-sm border border-emerald-500/30 bg-emerald-500/10 px-1 py-px text-[9px] font-medium uppercase tracking-wide text-emerald-600 dark:text-emerald-400">
                      Free
                    </span>
                  ) : null}
                </CommandItem>
              );
            })}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
