// Per-user shortcut override store.
//
// One interface, two implementations possible: the localStorage one
// shipped here (used by the dispatcher) and a future SQLite-backed
// one for cross-machine sync. The dispatcher should never assume
// which implementation it has — depend on the interface, swap impls
// at boot.

const STORAGE_KEY = "flowstate:keyboard-overrides";

/**
 * Override = "use this DSL string for shortcut id X instead of its
 * defaultBinding". `null` clears the override (back to default).
 *
 * `subscribe` is for the conflict-detection re-run + the future
 * rebinding UI's live re-render. The cheatsheet doesn't yet read
 * overrides directly (it reads through the registry), but having a
 * single subscribe channel here keeps everything reactive when it
 * does.
 */
export interface ShortcutOverrideStore {
  get(id: string): string | null;
  set(id: string, dsl: string | null): void;
  /** Snapshot every override (used by the cheatsheet + conflict
   *  detector to read everything in one pass). */
  all(): Record<string, string>;
  subscribe(cb: () => void): () => void;
}

/** Build a localStorage-backed store. Tests can pass their own
 *  in-memory implementation that satisfies the interface. */
export function createLocalStorageOverrideStore(): ShortcutOverrideStore {
  // In-memory mirror so reads are sync and parse-once. Re-hydrated
  // from localStorage on construction; writes flush back atomically.
  // Using a single JSON blob (not one entry per id) keeps the read
  // path a single localStorage hit at boot — matters for cold-start
  // perf on a slow disk.
  let cache: Record<string, string> = {};
  try {
    const raw =
      typeof window !== "undefined"
        ? window.localStorage.getItem(STORAGE_KEY)
        : null;
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
          if (typeof v === "string") cache[k] = v;
        }
      }
    }
  } catch {
    // Corrupt blob — ignore and start fresh. Worst case the user
    // re-binds; better than crashing the app on boot.
  }

  const listeners = new Set<() => void>();

  function flush(): void {
    if (typeof window === "undefined") return;
    try {
      if (Object.keys(cache).length === 0) {
        window.localStorage.removeItem(STORAGE_KEY);
      } else {
        window.localStorage.setItem(STORAGE_KEY, JSON.stringify(cache));
      }
    } catch {
      // Quota exceeded / private mode — overrides won't persist
      // across reload but the in-memory cache still serves the
      // current session.
    }
  }

  function emit(): void {
    for (const cb of listeners) cb();
  }

  return {
    get(id) {
      return cache[id] ?? null;
    },
    set(id, dsl) {
      if (dsl === null) {
        if (!(id in cache)) return;
        delete cache[id];
      } else {
        if (cache[id] === dsl) return;
        cache[id] = dsl;
      }
      flush();
      emit();
    },
    all() {
      // Defensive copy so callers can't mutate the cache directly.
      return { ...cache };
    },
    subscribe(cb) {
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
  };
}

// Module-level singleton — every call site (dispatcher, cheatsheet,
// future Settings rebinding row) reads from the same store. Lazy so
// SSR / test environments don't crash on the localStorage access at
// module import.
let singleton: ShortcutOverrideStore | null = null;
export function getOverrideStore(): ShortcutOverrideStore {
  if (singleton === null) {
    singleton = createLocalStorageOverrideStore();
  }
  return singleton;
}
