// Factory used by @pierre/diffs' WorkerPoolContextProvider. Vite
// recognises `new Worker(new URL(..., import.meta.url), { type: "module" })`
// as a module-worker import and emits a dedicated chunk for it, so
// we don't need the `?worker` suffix or any extra Vite plugin.
//
// The call site MUST be statically analyzable — don't inline this
// construction into a hook or conditional; keep it in its own
// module so the URL string stays next to the new Worker() literal.
export function createPierreDiffsWorker(): Worker {
  return new Worker(
    new URL("@pierre/diffs/worker/worker.js", import.meta.url),
    { type: "module" },
  );
}

// ─────────────────────────────────────────────────────────────────
// Worker pool size — user-configurable from Settings → Performance
// ─────────────────────────────────────────────────────────────────
//
// The @pierre/diffs pool is a singleton built in main.tsx at app
// boot, shared between the diff panel and the /code view's per-file
// viewer. More workers = more parallel Shiki tokenization on big
// diffs, at the cost of resident memory (each worker holds its own
// Shiki + the pre-warmed grammars from main.tsx, ~20-50 MB).
//
// Workers are message-passing: they consume zero CPU when idle, so
// the only ongoing cost is memory. There is no background work.
//
// The setting persists in flowzen's own SQLite via the Tauri
// `get_user_config` / `set_user_config` commands — separate from
// the agent SDK's daemon persistence. SDK and app each own their
// own database; nothing about app-level config belongs in the
// daemon's schema.

import { getUserConfig, setUserConfig } from "./api";

// user_config table key for the chosen pool size.
export const POOL_SIZE_CONFIG_KEY = "pierre_diffs.pool_size";

// Hard floor: at least one worker so highlighting works at all.
export const POOL_SIZE_MIN = 1;

// Hard ceiling: 2× the machine's logical core count. Going higher
// just wastes memory — Shiki is CPU-bound and there's no benefit
// to more workers than the OS can run in parallel × 2.
export function getMaxPoolSize(): number {
  const cores =
    (typeof navigator !== "undefined" && navigator.hardwareConcurrency) || 4;
  return Math.max(POOL_SIZE_MIN, cores * 2);
}

// Default if the user hasn't touched the setting. min(8, max) so:
//   - 1-2 core machines get 2-4 workers (their max), not 8
//   - 4-8 core machines get 8 workers (the comfort cap)
//   - 16+ core machines also get 8 workers — we don't go big-by-
//     default; users can opt in to higher via Settings.
export function getDefaultPoolSize(): number {
  return Math.min(8, getMaxPoolSize());
}

function clampPoolSize(value: number): number {
  return Math.max(
    POOL_SIZE_MIN,
    Math.min(getMaxPoolSize(), Math.round(value)),
  );
}

// Read + clamp + fallback. Async because the value lives in
// flowzen's SQLite (via Tauri IPC, not localStorage). Resolves
// quickly — local SQLite read is sub-millisecond — but main.tsx
// has to await it before mounting the worker pool provider.
export async function readPoolSizeSetting(): Promise<number> {
  try {
    const raw = await getUserConfig(POOL_SIZE_CONFIG_KEY);
    if (raw === null) return getDefaultPoolSize();
    const parsed = Number.parseInt(raw, 10);
    if (!Number.isFinite(parsed)) return getDefaultPoolSize();
    return clampPoolSize(parsed);
  } catch {
    return getDefaultPoolSize();
  }
}

export async function writePoolSizeSetting(size: number): Promise<void> {
  try {
    await setUserConfig(POOL_SIZE_CONFIG_KEY, String(clampPoolSize(size)));
  } catch {
    /* storage may be unavailable */
  }
}
