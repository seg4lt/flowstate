import { beforeEach, describe, expect, it } from "vitest";
import {
  clampEffortToModel,
  clampThinkingModeToModel,
  EFFORT_ORDER,
  MODEL_GATED_EFFORT_LEVELS,
  readPickedModel,
  rememberPickedModel,
} from "./model-settings";
import type { ProviderModel } from "./types";

// Minimal in-memory sessionStorage shim so the tests run under
// `--environment=node` (vitest's jsdom env fails to start on this
// machine due to an unrelated ESM infra bug — see the commit
// message).
class MemoryStorage {
  private store = new Map<string, string>();
  getItem(key: string): string | null {
    return this.store.has(key) ? (this.store.get(key) as string) : null;
  }
  setItem(key: string, value: string): void {
    this.store.set(key, value);
  }
  removeItem(key: string): void {
    this.store.delete(key);
  }
  clear(): void {
    this.store.clear();
  }
}
// @ts-expect-error — attaching to the global for the duration of the tests.
globalThis.sessionStorage = new MemoryStorage();

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

describe("rememberPickedModel / readPickedModel", () => {
  beforeEach(() => {
    // Reset between tests so state from one case doesn't leak.
    (globalThis.sessionStorage as unknown as MemoryStorage).clear();
  });

  it("round-trips a picked alias per session id", () => {
    rememberPickedModel("sess-1", "sonnet");
    expect(readPickedModel("sess-1")).toBe("sonnet");
  });

  it("returns undefined for a session that never had a model remembered", () => {
    expect(readPickedModel("sess-never")).toBeUndefined();
  });

  it("keeps per-session entries independent", () => {
    rememberPickedModel("sess-1", "sonnet");
    rememberPickedModel("sess-2", "default");
    expect(readPickedModel("sess-1")).toBe("sonnet");
    expect(readPickedModel("sess-2")).toBe("default");
  });

  it("overwrites when the user picks a different alias on the same session", () => {
    rememberPickedModel("sess-1", "sonnet");
    rememberPickedModel("sess-1", "haiku");
    expect(readPickedModel("sess-1")).toBe("haiku");
  });
});
