import { ChevronDown, Check } from "lucide-react";
import { Button } from "@/components/ui/button";
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

  if (models.length === 0) return null;

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
        <Button variant="ghost" size="xs">
          {currentLabel}
          <ChevronDown className="ml-0.5 h-3 w-3 text-muted-foreground" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent side="top" align="start" className="min-w-36">
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
