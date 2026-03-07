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
