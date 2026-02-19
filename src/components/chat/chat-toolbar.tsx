import { ModelSelector } from "./model-selector";
import { EffortSelector } from "./effort-selector";
import { ModeSelector } from "./mode-selector";
import { ContextDisplay } from "./context-display";
import { useApp } from "@/stores/app-store";
import type { ProviderKind, ReasoningEffort, PermissionMode } from "@/lib/types";

interface ChatToolbarProps {
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
  effort: ReasoningEffort;
  onEffortChange: (effort: ReasoningEffort) => void;
  permissionMode: PermissionMode;
  onPermissionModeChange: (mode: PermissionMode) => void;
}

export function ChatToolbar({
  sessionId,
  provider,
  currentModel,
  effort,
  onEffortChange,
  permissionMode,
  onPermissionModeChange,
}: ChatToolbarProps) {
  const { state } = useApp();
  const providerLabel = state.providers.find((p) => p.kind === provider)?.label;

  return (
    <div className="flex items-center gap-1.5">
      <ModelSelector
        sessionId={sessionId}
        provider={provider}
        currentModel={currentModel}
      />
      <EffortSelector value={effort} onChange={onEffortChange} />
      <ModeSelector value={permissionMode} onChange={onPermissionModeChange} />
      {providerLabel && (
        <span className="text-xs text-muted-foreground">{providerLabel}</span>
      )}
      <div className="ml-auto">
        <ContextDisplay />
      </div>
    </div>
  );
}
