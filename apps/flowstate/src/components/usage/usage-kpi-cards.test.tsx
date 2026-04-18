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
    expect(screen.getByText("—")).toBeInTheDocument();
  });
});
