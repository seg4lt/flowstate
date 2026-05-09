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
  /** Draft mode = there is no session yet (we're at /chat/draft/...).
   *  The provider chip mutates parent state instead of firing
   *  `update_session_provider`, and session-bound chips
   *  (ContextDisplay, GoalChip, ModelSelector's update_session_model)
   *  are hidden. Defaults to "active" so existing call sites keep
   *  their semantics. */
  mode?: "draft" | "active";
  /** Required in active mode; ignored / may be empty in draft mode. */
  sessionId: string;
  provider: ProviderKind;
  currentModel: string | undefined;
  /** Draft-mode callbacks. The provider chip uses
   *  `onProviderChange(kind, defaultModel?)` to update parent state in
   *  draft mode; ignored in active mode (the chip fires the wire
   *  message itself and only notifies via the same callback after the
   *  ack lands, so the parent stays in lock-step). */
  onProviderChange?: (provider: ProviderKind, defaultModel?: string) => void;
  /** Draft-mode model selection. The active-mode ModelSelector uses
   *  `update_session_model` directly; in draft mode the parent owns
   *  the value, so the chip needs a callback. */
  onModelChange?: (model: string) => void;
  effort: ReasoningEffort;
  onEffortChange: (effort: ReasoningEffort) => void;
  thinkingMode: ThinkingMode;
  onThinkingModeChange: (mode: ThinkingMode) => void;
  permissionMode: PermissionMode;
  onPermissionModeChange: (mode: PermissionMode) => void;
}

export function ChatToolbar({
  mode = "active",
  sessionId,
  provider,
  currentModel,
  onProviderChange,
  onModelChange,
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
  //
  // In draft mode there's no sessionId yet, so the picked-alias
  // lookup is a no-op and falls through to currentModel.
  const pickedModel = sessionId ? readPickedModel(sessionId) : undefined;
  const modelEntry = resolveModelDisplay(
    pickedModel ?? currentModel,
    provider,
    state.providers,
  ).entry;
  const supportedEffortLevels = modelEntry?.supportedEffortLevels ?? [];

  return (
    <div className="flex items-center gap-1.5">
      {/* Provider chip lives next to the model chip — picking a
          different provider naturally cascades into a default model
          for that provider. In active mode, the chip fires
          `update_session_provider`; in draft mode it just mutates
          parent state. */}
      <ProviderSelector
        mode={mode}
        provider={provider}
        sessionId={mode === "active" ? sessionId : undefined}
        onProviderChange={(p, m) => {
          onProviderChange?.(p, m);
        }}
      />
      <ModelSelector
        mode={mode}
        sessionId={sessionId}
        provider={provider}
        currentModel={currentModel}
        onModelChange={onModelChange}
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
          // Per-model gate on top of the provider-level
          // `thinkingEffort` flag. When the active model doesn't
          // advertise `supportsAdaptiveThinking`, the Adaptive pill
          // renders disabled (see ThinkingModeSelector) — and
          // chat-view's clamp effect auto-flips a stale `adaptive`
          // stored value to `always` on model change, so the
          // disabled pill never ends up looking "selected".
          // Defaults to `false` when we don't have a catalog entry
          // yet (bootstrap) to fail safe — showing Adaptive enabled
          // for a model whose capability we don't know risks a
          // silent SDK rejection on the next turn.
          supportsAdaptive={modelEntry?.supportsAdaptiveThinking ?? false}
        />
      )}
      <ModeSelector
        value={permissionMode}
        onChange={onPermissionModeChange}
        features={features}
      />
      {/* Goal chip: display + create/pause/resume/clear controls. Gated
          on `goalTracking` so providers without a goal-tracking primitive
          (everyone but Codex today) don't expose a non-functional
          affordance — the chip stays absent rather than rendering a
          control that would error on click. Hidden in draft mode —
          there's no session yet for a goal to live on. */}
      {mode === "active" && features.goalTracking && (
        <GoalChip sessionId={sessionId} />
      )}
      {/* Context display also requires a live session. */}
      {mode === "active" && showContextDisplay && (
        <div className="ml-auto">
          <ContextDisplay sessionId={sessionId} />
        </div>
      )}
    </div>
  );
}
