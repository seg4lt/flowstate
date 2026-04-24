import { describe, expect, it } from "vitest";
import { resolveModelDisplay } from "./model-lookup";
import type { ProviderModel, ProviderStatus } from "./types";

function providerModel(overrides: Partial<ProviderModel> = {}): ProviderModel {
  return {
    value: "claude-sonnet-4-5",
    label: "Claude Sonnet 4.5",
    supportsEffort: true,
    supportedEffortLevels: ["low", "medium", "high"],
    supportsAdaptiveThinking: true,
    supportsAutoMode: true,
    isFree: false,
    ...overrides,
  };
}

function claudeProvider(models: ProviderModel[]): ProviderStatus {
  return {
    kind: "claude",
    label: "Claude",
    status: "ready",
    enabled: true,
    models,
  } as ProviderStatus;
}

describe("resolveModelDisplay", () => {
  it("exact match returns the catalog entry", () => {
    const m = providerModel({ value: "claude-sonnet-4-5-20250929" });
    const out = resolveModelDisplay(
      "claude-sonnet-4-5-20250929",
      "claude",
      [claudeProvider([m])],
    );
    expect(out.entry).toBe(m);
    expect(out.label).toBe("Claude Sonnet 4.5");
    expect(out.rawId).toBe("claude-sonnet-4-5-20250929");
  });

  it("falls back to date-stripped match when session resolves to a different pinned date", () => {
    // Catalog was cached with one date stamp; SDK then resolves the
    // turn to a newer pinned id. Both sides collapse to the same
    // alias, so we should still surface the capability entry.
    const catalogEntry = providerModel({
      value: "claude-sonnet-4-5-20250929",
      label: "Claude Sonnet 4.5",
    });
    const out = resolveModelDisplay(
      "claude-sonnet-4-5-20251015",
      "claude",
      [claudeProvider([catalogEntry])],
    );
    expect(out.entry).toBe(catalogEntry);
    expect(out.label).toBe("Claude Sonnet 4.5");
    // rawId is preserved as the session's actual id — we only
    // fall back for the catalog lookup, not for the wire value.
    expect(out.rawId).toBe("claude-sonnet-4-5-20251015");
  });

  it("falls back when catalog carries the alias and session carries a pinned id", () => {
    const catalogEntry = providerModel({
      value: "claude-sonnet-4-5",
      label: "Claude Sonnet 4.5",
    });
    const out = resolveModelDisplay(
      "claude-sonnet-4-5-20250929",
      "claude",
      [claudeProvider([catalogEntry])],
    );
    expect(out.entry).toBe(catalogEntry);
  });

  it("falls back when catalog carries a pinned id and session carries the alias", () => {
    const catalogEntry = providerModel({
      value: "claude-sonnet-4-5-20250929",
      label: "Claude Sonnet 4.5",
    });
    const out = resolveModelDisplay(
      "claude-sonnet-4-5",
      "claude",
      [claudeProvider([catalogEntry])],
    );
    expect(out.entry).toBe(catalogEntry);
  });

  it("returns entry undefined when no catalog entry matches (different family)", () => {
    const opus = providerModel({
      value: "claude-opus-4-5-20250929",
      label: "Claude Opus 4.5",
    });
    const out = resolveModelDisplay(
      "claude-sonnet-4-5-20250929",
      "claude",
      [claudeProvider([opus])],
    );
    expect(out.entry).toBeUndefined();
    // Label degrades to the raw id so the UI still shows something.
    expect(out.label).toBe("claude-sonnet-4-5-20250929");
    expect(out.rawId).toBe("claude-sonnet-4-5-20250929");
  });

  it("picks the correct entry from a multi-model catalog via fallback", () => {
    const sonnet = providerModel({
      value: "claude-sonnet-4-5",
      label: "Claude Sonnet 4.5",
    });
    const opus = providerModel({
      value: "claude-opus-4-5-20250929",
      label: "Claude Opus 4.5",
    });
    const haiku = providerModel({
      value: "claude-haiku-4-5",
      label: "Claude Haiku 4.5",
    });
    const out = resolveModelDisplay(
      "claude-opus-4-5-20251015",
      "claude",
      [claudeProvider([sonnet, opus, haiku])],
    );
    expect(out.entry).toBe(opus);
  });

  it("returns empty/undefined when modelId is undefined", () => {
    const m = providerModel();
    const out = resolveModelDisplay(undefined, "claude", [claudeProvider([m])]);
    expect(out.entry).toBeUndefined();
    expect(out.rawId).toBe("");
    expect(out.label).toBe("");
  });

  it("returns empty/undefined when provider isn't in the list", () => {
    const m = providerModel();
    const out = resolveModelDisplay("claude-sonnet-4-5", "codex", [
      claudeProvider([m]),
    ]);
    expect(out.entry).toBeUndefined();
    expect(out.providerLabel).toBe("");
    // Label still falls back to the raw id so the UI shows something.
    expect(out.label).toBe("claude-sonnet-4-5");
  });

  it("does not strip non-8-digit trailing numbers (avoids false matches)", () => {
    // Guard against over-eager stripping: only a full YYYYMMDD suffix
    // should be collapsed. A 4-digit year like `-2025` on its own
    // shouldn't match a pinned id with `-20250929`.
    const catalogEntry = providerModel({
      value: "some-model-2025",
      label: "Some Model 2025",
    });
    const out = resolveModelDisplay(
      "some-model-20250929",
      "claude",
      [claudeProvider([catalogEntry])],
    );
    // Under the current regex, target strips to "some-model" and the
    // catalog entry strips to "some-model-2025" — NO match. Correct.
    expect(out.entry).toBeUndefined();
  });

  // ─── Family-heuristic fallback (the SDK's branded-alias catalog) ──

  // Catalog shape the Claude SDK actually returns from
  // `q.supportedModels()` — four branded aliases. Captured verbatim
  // from the SDK probe so the tests track the real contract, not my
  // guess at it.
  const SDK_CATALOG: ProviderModel[] = [
    providerModel({
      value: "default",
      label: "Default (recommended)",
      supportedEffortLevels: ["low", "medium", "high", "xhigh", "max"],
      supportsAdaptiveThinking: true,
      supportsAutoMode: true,
    }),
    providerModel({
      value: "sonnet",
      label: "Sonnet",
      supportedEffortLevels: ["low", "medium", "high", "max"],
      supportsAdaptiveThinking: true,
    }),
    providerModel({
      value: "sonnet[1m]",
      label: "Sonnet (1M context)",
      supportedEffortLevels: ["low", "medium", "high", "max"],
      supportsAdaptiveThinking: true,
    }),
    providerModel({
      value: "haiku",
      label: "Haiku",
      supportsEffort: false,
      supportedEffortLevels: [],
      supportsAdaptiveThinking: false,
    }),
  ];

  it("maps a pinned opus id onto the `default` catalog entry", () => {
    // This is the exact case from the bug screenshot: session.model
    // is `claude-opus-4-7[1m]` but the catalog only knows aliases.
    const out = resolveModelDisplay(
      "claude-opus-4-7[1m]",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("default");
    expect(out.entry?.supportedEffortLevels).toContain("xhigh");
    expect(out.entry?.supportedEffortLevels).toContain("max");
    expect(out.entry?.supportsAdaptiveThinking).toBe(true);
    // The chip label upgrades from the raw id to the branded alias
    // so the toolbar reads "Default (recommended)" instead of
    // `claude-opus-4-7[1m]`.
    expect(out.label).toBe("Default (recommended)");
  });

  it("maps a plain opus pinned id (no [1m]) onto default", () => {
    const out = resolveModelDisplay(
      "claude-opus-4-7-20250514",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("default");
  });

  it("prefers sonnet[1m] when the target carries [1m]", () => {
    const out = resolveModelDisplay(
      "claude-sonnet-4-6[1m]",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("sonnet[1m]");
  });

  it("prefers plain sonnet when the target has no [1m]", () => {
    const out = resolveModelDisplay(
      "claude-sonnet-4-6-20251015",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("sonnet");
  });

  it("maps any haiku id onto the haiku catalog entry", () => {
    const out = resolveModelDisplay(
      "claude-haiku-4-5-20250929",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("haiku");
  });

  it("returns undefined for an id with no recognisable family", () => {
    const out = resolveModelDisplay(
      "gpt-5-turbo",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry).toBeUndefined();
  });

  it("family fallback is case-insensitive", () => {
    // Defensive: some id forms might arrive capitalised.
    const out = resolveModelDisplay(
      "Claude-OPUS-4-7[1M]",
      "claude",
      [claudeProvider(SDK_CATALOG)],
    );
    expect(out.entry?.value).toBe("default");
  });

  it("falls back gracefully when the catalog lacks the preferred alias", () => {
    // Catalog only has `sonnet[1m]` (not plain `sonnet`). A target
    // without `[1m]` should still resolve to the 1M variant rather
    // than returning undefined — better to surface some capability
    // info than none.
    const catalog = SDK_CATALOG.filter((m) => m.value !== "sonnet");
    const out = resolveModelDisplay(
      "claude-sonnet-4-6",
      "claude",
      [claudeProvider(catalog)],
    );
    expect(out.entry?.value).toBe("sonnet[1m]");
  });
});
