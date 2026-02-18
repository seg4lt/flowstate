import { ModelSelector } from "./model-selector";
import { EffortSelector } from "./effort-selector";
import { ModeSelector } from "./mode-selector";
import { ContextDisplay } from "./context-display";
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

const ROUTING_PROVIDER_LABELS: Record<ProviderKind, string> = {
  codex: "OpenAI",
  claude: "Anthropic",
  github_copilot: "GitHub Copilot",
  claude_cli: "Anthropic",
  github_copilot_cli: "GitHub Copilot",
};

// Best-effort heuristic: derive the upstream provider from the model
// value, since a routing provider (e.g. GitHub Copilot CLI) can serve
// models from Anthropic, OpenAI, and Google interchangeably. Falls back
// to the routing provider's label when the model name is unknown.
function modelOriginProvider(
  modelValue: string | undefined,
  routingProvider: ProviderKind,
): string {
  if (modelValue) {
    const v = modelValue.toLowerCase();
    if (
      v === "sonnet" ||
      v === "opus" ||
      v === "haiku" ||
      v.startsWith("claude")
    ) {
      return "Anthropic";
    }
    if (
      v.startsWith("gpt") ||
      v.startsWith("o1") ||
      v.startsWith("o3") ||
      v.startsWith("o4")
    ) {
      return "OpenAI";
    }
    if (v.startsWith("gemini")) {
      return "Google";
    }
  }
  return ROUTING_PROVIDER_LABELS[routingProvider];
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
  const originProvider = modelOriginProvider(currentModel, provider);

  return (
    <div className="flex flex-col gap-0.5">
      <div className="flex items-center gap-1.5">
        <ModelSelector
          sessionId={sessionId}
          provider={provider}
          currentModel={currentModel}
        />
        <EffortSelector value={effort} onChange={onEffortChange} />
        <ModeSelector value={permissionMode} onChange={onPermissionModeChange} />
        <div className="ml-auto">
          <ContextDisplay />
        </div>
      </div>
      <div className="px-2 text-[10px] leading-none text-muted-foreground">
        {originProvider}
      </div>
    </div>
  );
}
