import { describe, expect, it } from "vitest";
import {
  clampEffortToModel,
  clampThinkingModeToModel,
  EFFORT_ORDER,
  MODEL_GATED_EFFORT_LEVELS,
} from "./model-settings";
import type { ProviderModel } from "./types";

// Helper: build a minimal ProviderModel for testing.
function model(overrides: Partial<ProviderModel> = {}): ProviderModel {
  return {
    value: "test-model",
    label: "Test Model",
    supportsEffort: true,
    supportedEffortLevels: [],
    supportsAdaptiveThinking: false,
    supportsAutoMode: false,
    isFree: false,
    ...overrides,
  };
}

describe("EFFORT_ORDER", () => {
  it("is ordered max → minimal (descending capability)", () => {
    expect(EFFORT_ORDER).toEqual([
      "max",
      "xhigh",
      "high",
      "medium",
      "low",
      "minimal",
    ]);
  });

  it("marks xhigh and max as model-gated", () => {
    expect(MODEL_GATED_EFFORT_LEVELS.has("xhigh")).toBe(true);
    expect(MODEL_GATED_EFFORT_LEVELS.has("max")).toBe(true);
    // The baseline levels are never gated.
    expect(MODEL_GATED_EFFORT_LEVELS.has("high")).toBe(false);
    expect(MODEL_GATED_EFFORT_LEVELS.has("medium")).toBe(false);
    expect(MODEL_GATED_EFFORT_LEVELS.has("low")).toBe(false);
    expect(MODEL_GATED_EFFORT_LEVELS.has("minimal")).toBe(false);
  });
});

describe("clampEffortToModel", () => {
  it("returns input unchanged when modelEntry is undefined (bootstrap)", () => {
    expect(clampEffortToModel("max", undefined)).toBe("max");
    expect(clampEffortToModel("xhigh", undefined)).toBe("xhigh");
    expect(clampEffortToModel("high", undefined)).toBe("high");
  });

  it("passes through non-gated levels regardless of supported list", () => {
    const m = model({ supportedEffortLevels: [] });
    expect(clampEffortToModel("high", m)).toBe("high");
    expect(clampEffortToModel("medium", m)).toBe("medium");
    expect(clampEffortToModel("low", m)).toBe("low");
    expect(clampEffortToModel("minimal", m)).toBe("minimal");
  });

  it("clamps max → high when no gated levels are advertised", () => {
    const m = model({ supportedEffortLevels: [] });
    expect(clampEffortToModel("max", m)).toBe("high");
  });

  it("clamps xhigh → high when no gated levels are advertised", () => {
    const m = model({ supportedEffortLevels: [] });
    expect(clampEffortToModel("xhigh", m)).toBe("high");
  });

  it("preserves max when the model advertises max", () => {
    const m = model({ supportedEffortLevels: ["xhigh", "max"] });
    expect(clampEffortToModel("max", m)).toBe("max");
  });

  it("preserves xhigh when the model advertises xhigh", () => {
    const m = model({ supportedEffortLevels: ["xhigh"] });
    expect(clampEffortToModel("xhigh", m)).toBe("xhigh");
  });

  it("steps max → xhigh when xhigh is advertised but max isn't", () => {
    const m = model({ supportedEffortLevels: ["xhigh"] });
    expect(clampEffortToModel("max", m)).toBe("xhigh");
  });

  it("steps max → high when neither xhigh nor max is advertised", () => {
    const m = model({ supportedEffortLevels: [] });
    expect(clampEffortToModel("max", m)).toBe("high");
  });

  it("respects a model that advertises only max (skips xhigh step)", () => {
    // Unusual but possible: a model accepts max but not xhigh.
    const m = model({ supportedEffortLevels: ["max"] });
    // From xhigh, walk DOWN — max is above xhigh so it isn't considered.
    // First accepted level below xhigh is `high` (non-gated).
    expect(clampEffortToModel("xhigh", m)).toBe("high");
    // From max, max itself is accepted.
    expect(clampEffortToModel("max", m)).toBe("max");
  });
});

describe("clampThinkingModeToModel", () => {
  it("returns input unchanged when modelEntry is undefined", () => {
    expect(clampThinkingModeToModel("adaptive", undefined)).toBe("adaptive");
    expect(clampThinkingModeToModel("always", undefined)).toBe("always");
  });

  it("keeps adaptive when the model supports it", () => {
    const m = model({ supportsAdaptiveThinking: true });
    expect(clampThinkingModeToModel("adaptive", m)).toBe("adaptive");
  });

  it("clamps adaptive → always on a model that doesn't support adaptive", () => {
    const m = model({ supportsAdaptiveThinking: false });
    expect(clampThinkingModeToModel("adaptive", m)).toBe("always");
  });

  it("always is accepted by every model (pass-through)", () => {
    const supports = model({ supportsAdaptiveThinking: true });
    const noSupport = model({ supportsAdaptiveThinking: false });
    expect(clampThinkingModeToModel("always", supports)).toBe("always");
    expect(clampThinkingModeToModel("always", noSupport)).toBe("always");
  });
});
