import React from "react";
import ReactDOM from "react-dom/client";
import { RouterProvider } from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { WorkerPoolContextProvider } from "@pierre/diffs/react";
import { router } from "./router";
import {
  createPierreDiffsWorker,
  getDefaultPoolSize,
  readPoolSizeSetting,
} from "@/lib/pierre-diffs-worker";
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

// Async boot: we read the user's chosen highlighter pool size from
// the flowstate-app-owned SQLite (via Tauri IPC) before mounting the
// worker pool provider. The pool is a singleton built at provider
// mount, so we can't change it after — the value has to be in hand
// before the first React render. Local SQLite reads are
// sub-millisecond; the IPC roundtrip is the only cost (a few ms),
// so the perceived startup delay is invisible.
//
// If the read fails for any reason (Tauri not ready, SQLite locked,
// corrupt value), we fall back to the default. The fallback path
// keeps the app launchable even if the config store is wedged.
async function bootstrap() {
  let poolSize: number;
  try {
    poolSize = await readPoolSizeSetting();
  } catch {
    poolSize = getDefaultPoolSize();
  }

  // Wrap the router in @pierre/diffs' worker pool so every diff
  // view tokenises and diffs off the main thread. Without this,
  // Shiki + Myers run inline and a single large file locks up the
  // UI for seconds. Singleton pool is cheap and is shared across
  // every route / session switch (diff panel + /code view both
  // consume it).
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <QueryClientProvider client={queryClient}>
        <WorkerPoolContextProvider
          poolOptions={{
            workerFactory: createPierreDiffsWorker,
            // User-configurable from Settings → Performance. Read
            // above before mounting; changing the setting requires
            // restarting Flowstate because the @pierre/diffs pool is
            // a singleton built at provider mount. More workers
            // tokenize big diffs in parallel at the cost of
            // resident memory; see pierre-diffs-worker.ts for the
            // bounds rationale.
            poolSize,
          }}
          highlighterOptions={{
            // ThemesType shape covers both dark and light — the
            // per-render `themeType` (set inside DiffBody's options)
            // picks which of these to apply. The worker just needs
            // both variants resolved up front.
            theme: { dark: "pierre-dark", light: "pierre-light" },
            // Pre-warm the grammars we actually hit so the first
            // diff render doesn't pay a "first time loading
            // TypeScript grammar" cost. Extend if a user hits an
            // uncommon language and sees a one-shot stall.
            langs: [
              "ts",
              "tsx",
              "js",
              "jsx",
              "rust",
              "toml",
              "json",
              "yaml",
              "md",
              "mdx",
              "css",
              "html",
              "python",
              "go",
              "java",
              "bash",
              "shell",
            ],
          }}
        >
          <RouterProvider router={router} />
        </WorkerPoolContextProvider>
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
