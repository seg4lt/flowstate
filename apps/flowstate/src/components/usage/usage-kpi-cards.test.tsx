import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { UsageKpiCards } from "./usage-kpi-cards";
import type { UsageTotals } from "@/lib/api";

function totals(overrides: Partial<UsageTotals> = {}): UsageTotals {
  return {
    turnCount: 0,
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    cacheWriteTokens: 0,
    totalCostUsd: 0,
    costHasUnknowns: false,
    totalDurationMs: 0,
    distinctSessions: 0,
    distinctModels: 0,
    ...overrides,
  };
}

describe("UsageKpiCards", () => {
  it("shows 'partial' badge when cost_has_unknowns is true", () => {
    render(
      <UsageKpiCards
        totals={totals({
          totalCostUsd: 1.23,
          costHasUnknowns: true,
          turnCount: 5,
        })}
      />,
    );
    expect(screen.getByText(/partial/i)).toBeInTheDocument();
  });

  it("hides 'partial' badge when all costs are known", () => {
    render(
      <UsageKpiCards
        totals={totals({
          totalCostUsd: 1.23,
          costHasUnknowns: false,
          turnCount: 5,
        })}
      />,
    );
    expect(screen.queryByText(/partial/i)).not.toBeInTheDocument();
  });

  it("formats costs under $0.01 as '<$0.01'", () => {
    render(
      <UsageKpiCards
        totals={totals({
          totalCostUsd: 0.003,
          turnCount: 1,
          distinctSessions: 1,
          distinctModels: 1,
        })}
      />,
    );
    expect(screen.getByText("<$0.01")).toBeInTheDocument();
  });

  it("computes average turn duration from totals", () => {
    render(
      <UsageKpiCards
        totals={totals({
          turnCount: 10,
          totalDurationMs: 50_000,
        })}
      />,
    );
    // 50000 / 10 = 5000ms → 5.0s
    expect(screen.getByText("5.0s")).toBeInTheDocument();
  });

  it("shows '—' for avg duration when no turns recorded", () => {
    render(<UsageKpiCards totals={totals()} />);
    // The empty-state KPI grid renders multiple "—" placeholders
    // (avg duration, cache hit, per-turn subtitles). We just want
    // to confirm at least one is present — not that there's exactly
    // one — so the assertion stays robust as we add more cards.
    expect(screen.getAllByText("—").length).toBeGreaterThan(0);
  });

  it("renders the eight KPI cards in the grid", () => {
    const { container } = render(
      <UsageKpiCards
        totals={totals({
          turnCount: 5,
          inputTokens: 100,
          outputTokens: 2_000,
          cacheReadTokens: 1_000_000,
          cacheWriteTokens: 50_000,
          totalCostUsd: 1.23,
          totalDurationMs: 25_000,
          distinctSessions: 2,
          distinctModels: 1,
        })}
      />,
    );
    // Exactly 8 cards: spend, turns, avg dur, cache hit, in, out,
    // cache read, cache write. Catches accidental row drops or
    // duplicates during refactors.
    const cards = container.querySelectorAll('[data-slot="usage-kpi-card"]');
    expect(cards.length).toBe(8);
  });

  it("surfaces cache hit % when cache activity is present", () => {
    render(
      <UsageKpiCards
        totals={totals({
          turnCount: 1,
          inputTokens: 100,
          cacheReadTokens: 9_900,
          cacheWriteTokens: 0,
        })}
      />,
    );
    // 9900 / (100 + 9900 + 0) = 99%
    expect(screen.getByText("99%")).toBeInTheDocument();
  });
});
