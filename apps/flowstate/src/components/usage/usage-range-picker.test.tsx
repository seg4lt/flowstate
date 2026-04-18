import { beforeEach, describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useUsageRange } from "./usage-range-picker";

const KEY = "flowstate:usage-range";

describe("useUsageRange", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("defaults to last30_days when nothing is persisted", () => {
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("hydrates from localStorage on mount", () => {
    window.localStorage.setItem(KEY, "last7_days");
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last7_days");
  });

  it("ignores garbage values in localStorage", () => {
    window.localStorage.setItem(KEY, "not_a_range");
    const { result } = renderHook(() => useUsageRange());
    expect(result.current[0]).toBe("last30_days");
  });

  it("persists updates back to localStorage", () => {
    const { result } = renderHook(() => useUsageRange());
    act(() => {
      result.current[1]("last90_days");
    });
    expect(result.current[0]).toBe("last90_days");
    expect(window.localStorage.getItem(KEY)).toBe("last90_days");
  });
});
