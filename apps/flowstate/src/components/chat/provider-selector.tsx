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
import { ALL_PROVIDERS } from "@/lib/providers";
import { ProviderIcon, PROVIDER_ICON_COLOR_CLASS } from "./provider-icon";
import { toast } from "@/hooks/use-toast";
import type { ProviderKind } from "@/lib/types";

interface ProviderSelectorProps {
  provider: ProviderKind;
  /** The session whose provider we're swapping. Always required —
   *  ChatToolbar only mounts on a live session now. */
  sessionId: string;
  /** Notify the parent AFTER the backend swap acks so the toolbar
   *  repaints in lock-step with the runtime. The optional second
   *  argument is the resolved default model for the new provider, so
   *  the parent can update its mirrored model state without a second
   *  round-trip. */
  onProviderChange: (provider: ProviderKind, defaultModel?: string) => void;
}

// Same threshold the model selector uses — keeps the two pickers
// visually consistent. Provider lists are tiny today (4 entries) so we
// never actually hit the search threshold, but the Popover + cmdk
// shape is identical.
const SEARCH_RELEVANT_PROVIDER_COUNT = 8;

/**
 * Toolbar provider chip. A pick fires `update_session_provider`,
 * which the runtime forwards to the new adapter's `start_session` so
 * any per-session bridge state is (re-)initialized with the existing
 * transcript intact.
 *
 * Mirrors `ModelSelector` (Popover + cmdk + autofocus invisible
 * input) so the two chips look and keyboard-navigate identically.
 */
export function ProviderSelector({
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
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-foreground/85 hover:text-foreground"
        >
          {showSpinner ? (
            <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
          ) : (
            <ProviderIcon
              kind={provider}
              className={`h-3.5 w-3.5 shrink-0 ${PROVIDER_ICON_COLOR_CLASS[provider]}`}
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
                  // Swaps need a healthy adapter on the other end —
                  // disable rows whose provider isn't ready so the
                  // user can't pick one and immediately hit an error.
                  disabled={!isReady}
                  onSelect={() => handleSelect(kind)}
                >
                  {isSelected ? (
                    <Check className="mr-2 h-3 w-3" />
                  ) : (
                    <span className="mr-2 w-3" />
                  )}
                  <ProviderIcon
                    kind={kind}
                    className={`mr-2 h-3.5 w-3.5 shrink-0 ${
                      isReady
                        ? PROVIDER_ICON_COLOR_CLASS[kind]
                        : "text-muted-foreground/40"
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
