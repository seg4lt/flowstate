import { ModelSelector } from "./model-selector";
import { EffortSelector } from "./effort-selector";
import { ThinkingModeSelector } from "./thinking-mode-selector";
import { ModeSelector } from "./mode-selector";
import { ContextDisplay } from "./context-display";
import { useApp } from "@/stores/app-store";
import { useContextDisplaySetting } from "@/hooks/use-context-display-setting";
import { useProviderFeatures } from "@/hooks/use-provider-features";
import { resolveModelDisplay } from "@/lib/model-lookup";
import type {
  ProviderKind,
  ReasoningEffort,
  PermissionMode,
  ThinkingMode,
} from "@/lib/types";

interface ChatToolbarProps {
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
  effort: ReasoningEffort;
  onEffortChange: (effort: ReasoningEffort) => void;
  thinkingMode: ThinkingMode;
  onThinkingModeChange: (mode: ThinkingMode) => void;
  permissionMode: PermissionMode;
  onPermissionModeChange: (mode: PermissionMode) => void;
}

export function ChatToolbar({
  sessionId,
  provider,
  currentModel,
  effort,
  onEffortChange,
  thinkingMode,
  onThinkingModeChange,
  permissionMode,
  onPermissionModeChange,
}: ChatToolbarProps) {
  const { state } = useApp();
  const { showContextDisplay } = useContextDisplaySetting();
  const features = useProviderFeatures(provider);
  const providerLabel = state.providers.find((p) => p.kind === provider)?.label;
  // Resolve the active model's capability record so the effort
  // selector can filter its options by what the model actually
  // supports. `supportedEffortLevels` comes from the Claude Agent
  // SDK's `ModelInfo.supportedEffortLevels`; it's empty when the
  // provider hasn't enumerated levels, which the selector treats as
  // "show flowstate's base set".
  const modelEntry = resolveModelDisplay(
    currentModel,
    provider,
    state.providers,
  ).entry;
  const supportedEffortLevels = modelEntry?.supportedEffortLevels ?? [];

  return (
    <div className="flex items-center gap-1.5">
      <ModelSelector
        sessionId={sessionId}
        provider={provider}
        currentModel={currentModel}
      />
      {/* Effort selector only renders for providers whose adapter
          honours reasoning_effort (Codex's turn/start payload, Claude
          SDK's thinking config). On Copilot/Claude-CLI the setting
          silently did nothing, so hiding it stops the user from
          tuning a control with no effect. */}
      {features.thinkingEffort && (
        <EffortSelector
          value={effort}
          onChange={onEffortChange}
          supportedEffortLevels={supportedEffortLevels}
        />
      )}
      {/* Thinking-mode dial (Always vs. Adaptive). Orthogonal to
          effort — effort is *how much*, mode is *when*. Gated on the
          same capability flag as the effort selector: only providers
          that honour thinking config (Claude Agent SDK today) get
          the control. Codex exposes `thinkingEffort` but its backend
          doesn't take an adaptive/always switch, so the value is
          silently ignored there — no dead control. */}
      {features.thinkingEffort && provider === "claude" && (
        <ThinkingModeSelector
          value={thinkingMode}
          onChange={onThinkingModeChange}
        />
      )}
      <ModeSelector
        value={permissionMode}
        onChange={onPermissionModeChange}
        features={features}
      />
      {providerLabel && (
        <span className="text-xs text-muted-foreground">{providerLabel}</span>
      )}
      {showContextDisplay && (
        <div className="ml-auto">
          <ContextDisplay sessionId={sessionId} />
        </div>
      )}
    </div>
  );
}
