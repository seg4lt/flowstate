import { ModelSelector } from "./model-selector";
import { ProviderSelector } from "./provider-selector";
import { EffortSelector } from "./effort-selector";
import { ThinkingModeSelector } from "./thinking-mode-selector";
import { ModeSelector } from "./mode-selector";
import { ContextDisplay } from "./context-display";
import { GoalChip } from "./goal-chip";
import { useApp } from "@/stores/app-store";
import { useContextDisplaySetting } from "@/hooks/use-context-display-setting";
import { useProviderFeatures } from "@/hooks/use-provider-features";
import { resolveModelDisplay } from "@/lib/model-lookup";
import { readPickedModel } from "@/lib/model-settings";
import type {
  ProviderKind,
  ReasoningEffort,
  PermissionMode,
  ThinkingMode,
} from "@/lib/types";

interface ChatToolbarProps {
  /** Live session this toolbar is bound to. Always present — the
   *  draft-mode ("no session yet") branch went away with the
   *  `/chat/draft/...` route; every entry point now eager-creates a
   *  real session before mounting the toolbar. */
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
  /** Optional notifier fired AFTER `update_session_provider` acks so
   *  the parent (ChatView) can refresh its mirrored provider/model
   *  state without waiting for the runtime event to round-trip. */
  onProviderChange?: (provider: ProviderKind, defaultModel?: string) => void;
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
  onProviderChange,
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
  // Resolve the active model's capability record so the effort
  // selector can filter its options by what the model actually
  // supports. `supportedEffortLevels` comes from the Claude Agent
  // SDK's `ModelInfo.supportedEffortLevels`; it's empty when the
  // provider hasn't enumerated levels, which the selector treats as
  // "show flowstate's base set".
  //
  // We try the user-picked alias first (`readPickedModel`), then
  // fall back to `currentModel`. The SDK's `supportedModels()`
  // returns aliases like `"default"`/`"sonnet"`, and on turn 1 the
  // SDK's `model_resolved` event replaces `session.model` with an
  // unrelated pinned id like `"claude-opus-4-7-20250514"` that has
  // no catalog entry — so without the picked-alias preference the
  // lookup would return `undefined`, collapsing
  // `supportedEffortLevels` to `[]` and disabling Adaptive on every
  // model after the first turn. The full rationale for the cache
  // lives in `lib/model-settings.ts`.
  const pickedModel = readPickedModel(sessionId);
  const modelEntry = resolveModelDisplay(
    pickedModel ?? currentModel,
    provider,
    state.providers,
  ).entry;
  const supportedEffortLevels = modelEntry?.supportedEffortLevels ?? [];

  // Whether the optional groups render — used to decide whether each
  // divider has neighbours on both sides. A divider with nothing to
  // its right (e.g. on Copilot, where the effort/thinking group is
  // hidden and Mode is the next chip) would be a stray vertical line.
  const hasReasoningGroup = features.thinkingEffort;
  return (
    <div className="flex min-w-0 items-center gap-0.5">
      {/* Provider + Model are the always-on left group. */}
      <ProviderSelector
        provider={provider}
        sessionId={sessionId}
        onProviderChange={(p, m) => {
          onProviderChange?.(p, m);
        }}
      />
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
      {hasReasoningGroup && (
        <>
          <ToolbarDivider />
          <EffortSelector
            value={effort}
            onChange={onEffortChange}
            supportedEffortLevels={supportedEffortLevels}
          />
          {/* Thinking-mode dial (Always vs. Adaptive). Orthogonal to
              effort — effort is *how much*, mode is *when*. Same
              capability gate as Effort, plus a Claude-only check
              because Codex's backend ignores the adaptive/always
              switch even though it honours thinkingEffort. */}
          {provider === "claude" && (
            <ThinkingModeSelector
              value={thinkingMode}
              onChange={onThinkingModeChange}
              // Per-model gate on top of the provider-level
              // `thinkingEffort` flag. When the active model doesn't
              // advertise `supportsAdaptiveThinking`, the Adaptive
              // pill renders disabled (see ThinkingModeSelector).
              supportsAdaptive={modelEntry?.supportsAdaptiveThinking ?? false}
            />
          )}
        </>
      )}
      <ToolbarDivider />
      <ModeSelector
        value={permissionMode}
        onChange={onPermissionModeChange}
        features={features}
      />
      {/* Goal chip: display + create/pause/resume/clear controls. Gated
          on `goalTracking` so providers without a goal-tracking primitive
          (everyone but Codex today) don't expose a non-functional
          affordance — the chip stays absent rather than rendering a
          control that would error on click. */}
      {features.goalTracking && <GoalChip sessionId={sessionId} />}
      {showContextDisplay && (
        <div className="ml-auto">
          <ContextDisplay sessionId={sessionId} />
        </div>
      )}
    </div>
  );
}

/**
 * Hairline vertical divider used between chip groups in the toolbar.
 * Mirrors the `|` glyph in the reference composer screenshot —
 * subtle enough to not draw the eye, present enough to visually group
 * related chips (Provider+Model | Effort+ThinkingMode | Mode+Goal).
 */
function ToolbarDivider() {
  return (
    <span
      aria-hidden
      className="mx-1 inline-block h-3.5 w-px shrink-0 bg-border/60"
    />
  );
}
