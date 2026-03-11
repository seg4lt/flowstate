import React from "react";
import ReactDOM from "react-dom/client";
import { RouterProvider } from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { WorkerPoolContextProvider } from "@pierre/diffs/react";
import { router } from "./router";
import { createPierreDiffsWorker } from "@/lib/pierre-diffs-worker";
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

// Wrap the router in @pierre/diffs' worker pool so every diff view
// (currently just the chat-side DiffPanel) tokenises and diffs off
// the main thread. Without this, Shiki + Myers run inline and a
// single large file locks up the UI for seconds. Singleton pool is
// cheap and is shared across every route / session switch.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
    <WorkerPoolContextProvider
      poolOptions={{
        workerFactory: createPierreDiffsWorker,
        // 4 parallel workers is plenty — the diff panel only ever
        // renders a couple of files concurrently thanks to its
        // IntersectionObserver lazy mount.
        poolSize: 4,
      }}
      highlighterOptions={{
        // ThemesType shape covers both dark and light — the
        // per-render `themeType` (set inside DiffBody's options)
        // picks which of these to apply. The worker just needs
        // both variants resolved up front.
        theme: { dark: "pierre-dark", light: "pierre-light" },
        // Pre-warm the grammars we actually hit so the first diff
        // render doesn't pay a "first time loading TypeScript
        // grammar" cost. Extend if a user hits an uncommon language
        // and sees a one-shot stall.
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
