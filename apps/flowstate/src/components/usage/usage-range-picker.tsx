import * as React from "react";
import { CalendarRange } from "lucide-react";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { customRange, isCustomRange, type UsageRange } from "@/lib/api";
import { cn } from "@/lib/utils";

const RANGE_STORAGE_KEY = "flowstate:usage-range";
const DEFAULT_RANGE: UsageRange = "last30_days";

type PresetRange = Exclude<UsageRange, { custom: { from: string; to: string } }>;

const PRESETS: { value: PresetRange; label: string }[] = [
  { value: "last7_days", label: "7d" },
  { value: "last30_days", label: "30d" },
  { value: "last90_days", label: "90d" },
  { value: "last120_days", label: "120d" },
  { value: "last180_days", label: "180d" },
  { value: "all_time", label: "All time" },
];

const PRESET_VALUES = new Set<PresetRange>(PRESETS.map((p) => p.value));

function isPresetRange(value: unknown): value is PresetRange {
  return typeof value === "string" && PRESET_VALUES.has(value as PresetRange);
}

function isUsageRangeValue(value: unknown): value is UsageRange {
  if (isPresetRange(value)) return true;
  if (typeof value !== "object" || value === null) return false;
  const obj = value as Record<string, unknown>;
  if (!("custom" in obj)) return false;
  const c = obj.custom as Record<string, unknown> | undefined;
  return (
    typeof c === "object" &&
    c !== null &&
    typeof c.from === "string" &&
    typeof c.to === "string" &&
    isYmd(c.from) &&
    isYmd(c.to)
  );
}

const YMD_RE = /^\d{4}-\d{2}-\d{2}$/;
function isYmd(s: string): boolean {
  // Cheap shape check — the full validity check happens on the Rust
  // side (`parse_custom_bounds`). The regex is enough to keep
  // garbled localStorage payloads from re-hydrating into the
  // dashboard and immediately erroring; if a saved date is malformed
  // we fall back to the default range.
  if (!YMD_RE.test(s)) return false;
  const ts = Date.parse(`${s}T00:00:00Z`);
  return !Number.isNaN(ts);
}

/// Persisted as JSON so the discriminated union (string preset OR
/// `{ custom: { from, to } }`) round-trips through localStorage. An
/// older release stored a bare string only; the validator's
/// `isPresetRange` short-circuit handles legacy values transparently.
export function useUsageRange(): [UsageRange, (next: UsageRange) => void] {
  const [range, setRange] = React.useState<UsageRange>(() => {
    const saved = window.localStorage.getItem(RANGE_STORAGE_KEY);
    if (saved == null) return DEFAULT_RANGE;
    // Pre-JSON releases stored bare preset strings ("last7_days").
    // Try the raw string first to keep those users on their saved
    // preference; fall back to JSON.parse for the new `{ custom }` shape.
    if (isPresetRange(saved)) return saved;
    try {
      const parsed = JSON.parse(saved) as unknown;
      return isUsageRangeValue(parsed) ? parsed : DEFAULT_RANGE;
    } catch {
      return DEFAULT_RANGE;
    }
  });
  const update = React.useCallback((next: UsageRange) => {
    setRange(next);
    window.localStorage.setItem(
      RANGE_STORAGE_KEY,
      typeof next === "string" ? next : JSON.stringify(next),
    );
  }, []);
  return [range, update];
}

// `YYYY-MM-DD` for `n` days before today (UTC). Used to seed the
// custom date inputs the first time a user opens the popover so
// they're not staring at empty fields.
function todayUtc(): string {
  return new Date().toISOString().slice(0, 10);
}
function daysAgoUtc(n: number): string {
  const d = new Date();
  d.setUTCDate(d.getUTCDate() - n);
  return d.toISOString().slice(0, 10);
}

/// Human-readable label for a custom range chip. Compact
/// (`May 1 – May 31`) when both ends are in the same year as today;
/// `Mar 4, 2025 – Apr 12, 2026` across years.
function formatCustomLabel(from: string, to: string): string {
  const f = new Date(`${from}T00:00:00Z`);
  const t = new Date(`${to}T00:00:00Z`);
  const today = new Date();
  const sameYear = f.getUTCFullYear() === t.getUTCFullYear();
  const sameAsThisYear = sameYear && f.getUTCFullYear() === today.getUTCFullYear();
  const opts: Intl.DateTimeFormatOptions = sameAsThisYear
    ? { month: "short", day: "numeric", timeZone: "UTC" }
    : { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" };
  const fmt = new Intl.DateTimeFormat(undefined, opts);
  return `${fmt.format(f)} – ${fmt.format(t)}`;
}

export function UsageRangePicker({
  value,
  onChange,
}: {
  value: UsageRange;
  onChange: (next: UsageRange) => void;
}) {
  const isCustom = isCustomRange(value);
  return (
    <div
      role="radiogroup"
      aria-label="Usage time range"
      className="inline-flex items-center rounded-lg border border-border bg-background p-0.5"
    >
      {PRESETS.map((opt) => (
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
      <CustomRangeButton
        active={isCustom}
        currentFrom={isCustom ? value.custom.from : null}
        currentTo={isCustom ? value.custom.to : null}
        onApply={(from, to) => onChange(customRange(from, to))}
      />
    </div>
  );
}

/// "Custom" chip + popover with two date inputs and Apply.
///
/// The chip behaves like a radio in the parent group (aria-checked
/// reflects whether the active range is custom) but its onClick
/// opens a popover instead of immediately switching to a custom
/// range — there's no sensible default `[from, to]` we can pick on
/// behalf of the user, so the dashboard waits for an Apply.
///
/// Inputs are native `<input type="date">`. They emit
/// `YYYY-MM-DD` on every change, which is exactly what the Rust
/// `Custom` variant expects, so no conversion needed. `min` /
/// `max` are wired so the date input's own picker greys out
/// reversed ranges before submit (the Rust side validates the
/// same invariant defensively — see `parse_custom_bounds`).
function CustomRangeButton({
  active,
  currentFrom,
  currentTo,
  onApply,
}: {
  active: boolean;
  currentFrom: string | null;
  currentTo: string | null;
  onApply: (from: string, to: string) => void;
}) {
  const [open, setOpen] = React.useState(false);
  // Seed inputs from the active range when one is selected; otherwise
  // seed with [30 days ago, today] so the popover has reasonable
  // values to Apply. Re-seed every time the popover opens so
  // navigating between presets doesn't leave the inputs stale.
  const [from, setFrom] = React.useState<string>("");
  const [to, setTo] = React.useState<string>("");
  React.useEffect(() => {
    if (open) {
      setFrom(currentFrom ?? daysAgoUtc(30));
      setTo(currentTo ?? todayUtc());
    }
  }, [open, currentFrom, currentTo]);

  const valid =
    isYmd(from) && isYmd(to) && from <= to;
  const errorMsg =
    !isYmd(from) || !isYmd(to)
      ? null
      : from > to
        ? "Start date must be on or before end date."
        : null;

  const label = active
    ? currentFrom && currentTo
      ? formatCustomLabel(currentFrom, currentTo)
      : "Custom"
    : "Custom";

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        {/* Plain <button>, not the cva-wrapped <Button>: Radix's
            asChild Slot needs the child to forward refs, and our
            Button component doesn't (see message-model-info.tsx
            for the same workaround). Without this the trigger
            silently swallows clicks. We pull in `buttonVariants`
            to reuse the exact same chip styling as the preset
            buttons next to us. */}
        <button
          type="button"
          role="radio"
          aria-checked={active}
          aria-label={
            active && currentFrom && currentTo
              ? `Custom range: ${formatCustomLabel(currentFrom, currentTo)}`
              : "Custom date range"
          }
          className={cn(
            buttonVariants({
              variant: active ? "default" : "ghost",
              size: "sm",
            }),
          )}
        >
          <CalendarRange className="size-3.5" aria-hidden />
          {label}
        </button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-auto min-w-[18rem]">
        <div className="flex flex-col gap-3">
          <div className="flex flex-col gap-1">
            <label
              htmlFor="usage-custom-from"
              className="text-xs font-medium text-muted-foreground"
            >
              Start date
              <span className="ml-1 font-normal text-muted-foreground/70">
                (covers from 12:00 AM)
              </span>
            </label>
            <input
              id="usage-custom-from"
              type="date"
              value={from}
              max={to || undefined}
              onChange={(e) => setFrom(e.currentTarget.value)}
              className="h-8 rounded-md border border-input bg-background px-2 text-sm tabular-nums"
            />
          </div>
          <div className="flex flex-col gap-1">
            <label
              htmlFor="usage-custom-to"
              className="text-xs font-medium text-muted-foreground"
            >
              End date
              <span className="ml-1 font-normal text-muted-foreground/70">
                (covers through 11:59 PM)
              </span>
            </label>
            <input
              id="usage-custom-to"
              type="date"
              value={to}
              min={from || undefined}
              max={todayUtc()}
              onChange={(e) => setTo(e.currentTarget.value)}
              className="h-8 rounded-md border border-input bg-background px-2 text-sm tabular-nums"
            />
          </div>
          {errorMsg ? (
            <div
              role="alert"
              className="rounded-md bg-destructive/10 px-2 py-1 text-xs text-destructive"
            >
              {errorMsg}
            </div>
          ) : null}
          <div className="flex justify-end gap-1.5">
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setOpen(false)}
            >
              Cancel
            </Button>
            <Button
              size="sm"
              disabled={!valid}
              onClick={() => {
                if (!valid) return;
                onApply(from, to);
                setOpen(false);
              }}
            >
              Apply
            </Button>
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
