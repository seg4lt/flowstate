import { beforeEach, describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useUsageRange } from "./usage-range-picker";
import { customRange } from "@/lib/api";

const KEY = "flowstate:usage-range";

describe("useUsageRange", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("defaults to last30_days when nothing is persisted", () => {
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("hydrates a preset string from localStorage on mount", () => {
    window.localStorage.setItem(KEY, "last7_days");
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last7_days");
  });

  it("ignores garbage values in localStorage", () => {
    window.localStorage.setItem(KEY, "not_a_range");
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("persists preset updates back to localStorage as bare strings", () => {
    const { result } = renderHook(() => useUsageRange());
    act(() => {
      result.current[1]("last90_days");
    });
    expect(result.current[0]).toBe("last90_days");
    expect(window.localStorage.getItem(KEY)).toBe("last90_days");
  });

  it("persists a custom range as JSON and rehydrates the object", () => {
    const { result } = renderHook(() => useUsageRange());
    const custom = customRange("2026-01-01", "2026-02-15");
    act(() => {
      result.current[1](custom);
    });
    expect(result.current[0]).toEqual(custom);
    const raw = window.localStorage.getItem(KEY);
    expect(raw).toBe(JSON.stringify(custom));
    // Re-hydrate in a fresh hook to prove the round-trip works.
    const { result: reborn } = renderHook(() => useUsageRange());
    expect(reborn.current[0]).toEqual(custom);
  });

  it("rejects malformed custom-range JSON in localStorage", () => {
    window.localStorage.setItem(
      KEY,
      JSON.stringify({ custom: { from: "garbage", to: "2026-02-15" } }),
    );
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("rejects custom-range values missing required fields", () => {
    window.localStorage.setItem(KEY, JSON.stringify({ custom: { from: "2026-01-01" } }));
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("rejects unparseable JSON gracefully", () => {
    window.localStorage.setItem(KEY, "{not valid json");
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });
});
