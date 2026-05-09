import { useState } from "react";
import { ChevronDown, Check, Loader2 } from "lucide-react";
import { Command as CmdkPrimitive } from "cmdk";
import {
  Command,
  CommandEmpty,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useApp } from "@/stores/app-store";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { rememberPickedModel } from "@/lib/model-settings";
import { readDefaultModel } from "@/lib/defaults-settings";
import { FOCUS_CHAT_INPUT_EVENT } from "@/lib/keyboard-shortcuts";
import { ALL_PROVIDERS, PROVIDER_COLORS } from "@/lib/providers";
import { toast } from "@/hooks/use-toast";
import type { ProviderKind } from "@/lib/types";

interface ProviderSelectorProps {
  /** Draft mode = no session yet; mutations stay local. Active mode =
   *  the chip is bound to a live session and a pick fires
   *  `update_session_provider` over the wire. */
  mode: "draft" | "active";
  provider: ProviderKind;
  /** Required in active mode (the session whose provider we're
   *  swapping). Ignored in draft mode. */
  sessionId?: string;
  /** Notify the parent of the user's pick. In draft mode the parent
   *  uses this to update its `provider` / `model` state; in active
   *  mode the parent receives this AFTER the backend swap acks so the
   *  toolbar repaints in lock-step. The optional second argument is
   *  the resolved default model for the new provider — handy in draft
   *  mode so the model chip can update without an extra round-trip. */
  onProviderChange: (provider: ProviderKind, defaultModel?: string) => void;
}

// Same threshold the model selector uses — keeps the two pickers
// visually consistent. Provider lists are tiny today (4 entries) so we
// never actually hit the search threshold, but the Popover + cmdk
// shape is identical.
const SEARCH_RELEVANT_PROVIDER_COUNT = 8;

/**
 * Toolbar provider chip. In draft mode the parent owns provider /
 * model state — picking here is a local mutation. In active mode a
 * pick fires `update_session_provider`, which the runtime forwards to
 * the new adapter's `start_session` so any per-session bridge state is
 * (re-)initialized with the existing transcript intact.
 *
 * Mirrors `ModelSelector` (Popover + cmdk + autofocus invisible
 * input) so the two chips look and keyboard-navigate identically.
 */
export function ProviderSelector({
  mode,
  provider,
  sessionId,
  onProviderChange,
}: ProviderSelectorProps) {
  const { state, send } = useApp();
  const { isProviderEnabled } = useProviderEnabled();
  const [open, setOpen] = useState(false);
  const [pendingProvider, setPendingProvider] =
    useState<ProviderKind | null>(null);

  const providerMap = new Map(state.providers.map((p) => [p.kind, p]));
  const enabledProviders = ALL_PROVIDERS.filter(({ kind }) =>
    isProviderEnabled(kind),
  );
  const currentLabel =
    providerMap.get(provider)?.label ??
    ALL_PROVIDERS.find((p) => p.kind === provider)?.label ??
    provider;

  // Show a small spinner on the chip while a swap is in flight so the
  // user can see their pick is being applied (matches the
  // ModelSelector's loading-models pattern).
  const showSpinner = pendingProvider !== null;
  const showSearch = enabledProviders.length >= SEARCH_RELEVANT_PROVIDER_COUNT;

  async function handleSelect(next: ProviderKind) {
    setOpen(false);
    if (next === provider) return;

    // Resolve the new provider's model so callers (draft parent or
    // remote handler) can update the model chip without a round-trip.
    // Priority: user's saved default for this provider → first entry
    // of the cached catalog → undefined (let the adapter pick).
    const saved = await readDefaultModel(next);
    const fallback = providerMap.get(next)?.models[0]?.value;
    const resolvedModel = saved ?? fallback;

    if (mode === "draft") {
      onProviderChange(next, resolvedModel);
      return;
    }

    if (!sessionId) {
      // Defensive — active mode without a session id is a programmer
      // error. Toast and bail rather than silently dropping the pick.
      toast({
        title: "Cannot switch provider",
        description: "Active provider switch is missing a session id.",
        duration: 4000,
      });
      return;
    }

    setPendingProvider(next);
    try {
      const res = await send({
        type: "update_session_provider",
        session_id: sessionId,
        provider: next,
        model: resolvedModel,
      });
      if (res?.type === "error") {
        toast({
          title: "Failed to switch provider",
          description: res.message,
          duration: 4000,
        });
        return;
      }
      // Stash the resolved model alias so the toolbar's capability
      // lookups survive the SDK's `model_resolved` overwrite — same
      // contract `ModelSelector.handleSelect` uses.
      if (resolvedModel) {
        rememberPickedModel(sessionId, resolvedModel);
      }
      // Notify the parent so it can update its mirrored state without
      // waiting for the `session_provider_updated` runtime event to
      // round-trip through the app-store reducer.
      onProviderChange(next, resolvedModel);
    } finally {
      setPendingProvider(null);
    }
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs hover:bg-accent"
        >
          {showSpinner ? (
            <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
          ) : (
            <span
              className={`inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[provider]}`}
            />
          )}
          {currentLabel}
          <ChevronDown className="h-3 w-3 text-muted-foreground" />
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className={showSearch ? "w-72 p-0" : "min-w-48 p-0"}
        // Match ModelSelector's behavior: returning focus to the chip
        // after a keyboard pick is the wrong UX in a chat — the user
        // came from the composer and expects to keep typing.
        onCloseAutoFocus={(e) => {
          e.preventDefault();
          window.dispatchEvent(new CustomEvent(FOCUS_CHAT_INPUT_EVENT));
        }}
      >
        <Command
          filter={(value, search) =>
            value.toLowerCase().includes(search.toLowerCase()) ? 1 : 0
          }
        >
          {showSearch ? (
            <CommandInput placeholder="Search providers…" autoFocus />
          ) : (
            // Invisible focusable input so up/down/enter still works
            // when the visible search box is hidden — same trick the
            // ModelSelector uses for short lists.
            <CmdkPrimitive.Input
              autoFocus
              aria-label="Filter providers"
              className="sr-only"
            />
          )}
          <CommandList>
            <CommandEmpty>No providers match.</CommandEmpty>
            {enabledProviders.map(({ kind, label }) => {
              const info = providerMap.get(kind);
              const isReady = info?.status === "ready";
              const isSelected = kind === provider;
              const searchValue = `${label} ${kind}`;
              return (
                <CommandItem
                  key={kind}
                  value={searchValue}
                  // Active-mode swaps need an actual healthy adapter
                  // on the other end. Draft-mode picks can target a
                  // not-yet-ready provider — `start_session` itself
                  // will surface the error at first send.
                  disabled={mode === "active" && !isReady}
                  onSelect={() => handleSelect(kind)}
                >
                  {isSelected ? (
                    <Check className="mr-2 h-3 w-3" />
                  ) : (
                    <span className="mr-2 w-3" />
                  )}
                  <span
                    className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${
                      isReady ? PROVIDER_COLORS[kind] : "bg-muted-foreground/30"
                    }`}
                  />
                  <span className="flex-1 truncate">{label}</span>
                  {!isReady && info?.message && (
                    <span className="ml-2 shrink-0 text-[10px] text-muted-foreground">
                      {info.message}
                    </span>
                  )}
                </CommandItem>
              );
            })}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
