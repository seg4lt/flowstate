import * as React from "react";
import { Button } from "@/components/ui/button";
import type { UsageRange } from "@/lib/api";

const RANGE_STORAGE_KEY = "flowstate:usage-range";
const RANGE_LABELS: { value: UsageRange; label: string }[] = [
  { value: "last7_days", label: "7d" },
  { value: "last30_days", label: "30d" },
  { value: "last90_days", label: "90d" },
  { value: "last120_days", label: "120d" },
  { value: "last180_days", label: "180d" },
  { value: "all_time", label: "All time" },
];

const DEFAULT_RANGE: UsageRange = "last30_days";

function isUsageRange(value: unknown): value is UsageRange {
  return (
    value === "last7_days" ||
    value === "last30_days" ||
    value === "last90_days" ||
    value === "last120_days" ||
    value === "last180_days" ||
    value === "all_time"
  );
}

export function useUsageRange(): [UsageRange, (next: UsageRange) => void] {
  const [range, setRange] = React.useState<UsageRange>(() => {
    const saved = window.localStorage.getItem(RANGE_STORAGE_KEY);
    return isUsageRange(saved) ? saved : DEFAULT_RANGE;
  });
  const update = React.useCallback((next: UsageRange) => {
    setRange(next);
    window.localStorage.setItem(RANGE_STORAGE_KEY, next);
  }, []);
  return [range, update];
}

export function UsageRangePicker({
  value,
  onChange,
}: {
  value: UsageRange;
  onChange: (next: UsageRange) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Usage time range"
      className="inline-flex rounded-lg border border-border bg-background p-0.5"
    >
      {RANGE_LABELS.map((opt) => (
        <Button
          key={opt.value}
          variant={value === opt.value ? "default" : "ghost"}
          size="sm"
          role="radio"
          aria-checked={value === opt.value}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
        </Button>
      ))}
    </div>
  );
}
