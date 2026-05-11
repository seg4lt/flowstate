import React from "react";
import ReactDOM from "react-dom/client";
import { RouterProvider } from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { router } from "./router";
import {
  getDefaultPoolSize,
  getMaxPoolSize,
  POOL_SIZE_MIN,
  readPoolSizeSetting,
} from "@/lib/pierre-diffs-worker";
import { PierrePoolProvider } from "@/lib/pierre-pool-controller";
import { getHighlighter } from "@/lib/shiki-singleton";
import { checkForUpdate } from "@/lib/updater";
import "./index.css";

// Single QueryClient for the whole app. Defaults chosen for a
// local-first desktop app where we want perceived-instant thread
// switches: never auto-refetch on mount or focus, keep cached
// sessions around long enough to be useful across back-and-forth
// navigation, but let each query opt in to its own staleTime.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: Infinity,
      gcTime: 30 * 60 * 1000,
      refetchOnWindowFocus: false,
      refetchOnMount: false,
      refetchOnReconnect: false,
      retry: false,
    },
  },
});

// Async boot: we read the user's saved "pool size" value from
// flowstate-app-owned SQLite (via Tauri IPC) before mounting the
// PierrePoolProvider. That value is treated as the **upper bound**
// on the auto-scaled pool — the controller starts at 1 worker and
// scales up under queue pressure toward this ceiling. Reading the
// setting is sub-millisecond + IPC roundtrip; falling back to the
// default keeps the app launchable if the config store is wedged.
async function bootstrap() {
  let savedPoolSize: number;
  try {
    savedPoolSize = await readPoolSizeSetting();
  } catch {
    savedPoolSize = getDefaultPoolSize();
  }
  // Interpret the saved value as a ceiling, bounded by what the
  // machine can reasonably run. Users whose saved value is "1"
  // (the new default) effectively disable auto-scale; their setup
  // serializes all diff work through a single worker — which is
  // fine for most workloads.
  const maxPoolSize = Math.max(
    POOL_SIZE_MIN,
    Math.min(savedPoolSize, getMaxPoolSize()),
  );

  // PierrePoolProvider wraps @pierre/diffs' WorkerPoolContextProvider
  // with three memory-aware behaviors: start at 1 worker, scale up
  // under queue pressure, and unmount entirely after prolonged idle
  // so worker heaps are freed. See pierre-pool-controller.tsx for
  // the full rationale.
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <QueryClientProvider client={queryClient}>
        <PierrePoolProvider
          maxPoolSize={maxPoolSize}
          highlighterOptions={{
            // "shiki-js" uses the JS runtime's native regex engine —
            // smaller memory footprint than "shiki-wasm" (no embedded
            // Oniguruma WASM module per worker) at the cost of
            // somewhat slower first-tokenize on JavaScriptCore
            // (Safari/WKWebView/Tauri). For the default 1-worker
            // configuration, the memory savings dominate the latency
            // cost. Users who routinely open huge diffs can bump the
            // pool size from Settings and/or opt back into wasm if we
            // add that toggle later.
            preferredHighlighter: "shiki-js",
            // ThemesType shape covers both dark and light — the
            // per-render `themeType` (set inside DiffBody's options)
            // picks which of these to apply. The worker just needs
            // both variants resolved up front.
            theme: { dark: "pierre-dark", light: "pierre-light" },
            // Pre-warm only the grammars we hit most often so each
            // worker's resident memory stays small. Other languages
            // pay a one-shot ~50-200 ms compile cost on first use,
            // which is fine for an occasional foreign-language diff.
            langs: ["ts", "rust", "java", "md"],
          }}
        >
          {/*
            HighlighterWarmup intentionally dropped from boot — it
            mounts 48 offscreen <PatchDiff> components to JIT-warm
            every common language up front. That's a lot of startup
            CPU + memory for warmups the user may never hit this
            session. The first real diff pays a ~100-300 ms
            first-tokenize latency per language instead, which is
            acceptable and amortizes quickly.
          */}
          <RouterProvider router={router} />
        </PierrePoolProvider>
      </QueryClientProvider>
    </React.StrictMode>,
  );
}

void bootstrap();

// Fire-and-forget startup update check. Runs once a few seconds
// after boot so the network call doesn't compete with the rest of
// the app coming up. The updater singleton (src/lib/updater.ts)
// captures the result; if an update is available, <UpdateBanner />
// renders the install CTA. No-op + silent on errors so a flaky
// network never blocks startup. Settings exposes a manual
// "Check now" button that hits the same code path.
window.setTimeout(() => {
  void checkForUpdate();
}, 5000);

// Fire-and-forget Shiki preload. The main-thread highlighter used
// by <CodeBlock> (see code-block.tsx) pays a ~400-800 ms cold start
// the first time it's resolved (dynamic imports + Oniguruma WASM
// init + preloaded grammar compile). Previously this was deferred
// to first-block render, which made cold thread-switches feel slow:
// users land on the message list, see plain-text code, and watch
// the highlighter swap to colored ~half a second later. We now
// kick the singleton off during boot in the background:
//
//   * It's a `void` Promise — never awaited — so the React tree
//     mounts immediately and the app shell paints without waiting.
//   * The dynamic imports run in parallel with everything else
//     happening at startup (worker pool warmup, router setup,
//     SQLite hydration). On any device that isn't completely
//     CPU-starved, Shiki is warm before the user clicks a thread.
//   * Cost: the WASM blob + 7 preloaded grammars (~2-5 MB) become
//     resident a bit earlier than they otherwise would have.
//     That's amortized across the whole session — once initialized
//     the singleton lives until tab close. Users who never open a
//     chat still pay this; we accept that to make chat-heavy
//     workflows (the dominant case) feel instant.
//   * Failures are swallowed: the same lazy fallback in
//     <CodeBlockInner> still runs if the prewarm errors out, so a
//     bad import doesn't break code blocks — it just removes the
//     speedup.
//
// requestIdleCallback would defer until the main thread is idle,
// but it isn't available in WKWebView / Safari. setTimeout(0) is
// good enough: by the time the macrotask fires, the React root
// has begun rendering and the import chain runs alongside it.
window.setTimeout(() => {
  void getHighlighter().catch((err) => {
    console.warn("[shiki] background preload failed:", err);
  });
}, 0);
