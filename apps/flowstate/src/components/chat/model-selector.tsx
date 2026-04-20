import { ChevronDown, Check, Loader2 } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";

interface ModelSelectorProps {
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
}

export function ModelSelector({
  sessionId,
  provider,
  currentModel,
}: ModelSelectorProps) {
  const { state, send } = useApp();
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

  const currentLabel =
    models.find((m) => m.value === currentModel)?.label ?? currentModel ?? "Default";

  async function handleSelect(model: string) {
    await send({
      type: "update_session_model",
      session_id: sessionId,
      model,
    });
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs hover:bg-accent"
        >
          {currentLabel}
          <ChevronDown className="h-3 w-3 text-muted-foreground" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-52">
        {models.map((model) => (
          <DropdownMenuItem
            key={model.value}
            onClick={() => handleSelect(model.value)}
          >
            {currentModel === model.value && (
              <Check className="mr-2 h-3 w-3" />
            )}
            {currentModel !== model.value && <span className="mr-2 w-3" />}
            {model.label}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
