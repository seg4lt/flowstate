import type { ProviderKind, ProviderModel, ProviderStatus } from "@/lib/types";

export interface ResolvedModelDisplay {
  /** The raw provider-level model id, exactly as sent to the wire. */
  rawId: string;
  /** Human-readable display label for this model. Falls back to the
   *  raw id when the provider hasn't catalogued this specific
   *  pinned variant (common when the SDK resolves an alias to a
   *  date-stamped version we don't list). */
  label: string;
  /** Display label for the provider ("Claude", "Codex", etc.). */
  providerLabel: string;
  /** Full ProviderModel entry when one matches exactly — useful if
   *  callers also want capability fields. Undefined when unmatched. */
  entry?: ProviderModel;
}

/**
 * Resolve a raw provider model id into the display strings we want to
 * render. Both the model label and provider label degrade gracefully:
 * when a pinned variant isn't in the provider's catalog we still
 * display the raw id rather than an empty string.
 *
 * Centralising this here keeps `agent-message`, `message-model-info`,
 * and the subagent header from each re-implementing the same lookup.
 */
export function resolveModelDisplay(
  modelId: string | undefined,
  providerKind: ProviderKind | undefined,
  providers: ProviderStatus[],
): ResolvedModelDisplay {
  const provider = providerKind
    ? providers.find((p) => p.kind === providerKind)
    : undefined;
  const entry = modelId
    ? provider?.models.find((m) => m.value === modelId)
    : undefined;
  return {
    rawId: modelId ?? "",
    label: entry?.label ?? modelId ?? "",
    providerLabel: provider?.label ?? "",
    entry,
  };
}
