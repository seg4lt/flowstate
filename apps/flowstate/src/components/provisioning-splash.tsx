import * as React from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useApp } from "@/stores/app-store";

// Shape of the `provision` events emitted by the Tauri shell during
// first launch. Matches `flowstate_app_layer::provision::ProvisionEvent`
// on the Rust side — serde renames the enum as `{ kind: "…", … }` via
// `#[serde(tag = "kind", rename_all = "kebab-case")]`.
type ProvisionEvent =
  | { kind: "started"; phase: string; message: string }
  | { kind: "completed"; phase: string; duration_ms: number }
  | { kind: "all-done"; duration_ms: number }
  | { kind: "failed"; phase: string; error: string };

/**
 * Full-screen loading overlay shown while the daemon runs its
 * first-launch provisioning steps (downloading Node.js, running
 * `npm install` for the provider SDKs). Hides as soon as the runtime's
 * `welcome` message arrives and the app flips to `ready: true`.
 *
 * Why this exists: on first launch the daemon needs 30–90 seconds to
 * download the embedded Node runtime + hydrate ~350 MB of provider
 * `node_modules` from npmjs.org. Without this overlay the user stares
 * at an empty greyed-out UI wondering if the app is broken. The splash
 * surfaces what's actually happening so the wait is honest.
 *
 * Behavior on warm launches: `provision_runtimes()` still runs but
 * every phase is a sub-millisecond sentinel hit, so this component
 * never gets a chance to render (the 300ms `showAfter` timer below
 * swallows the flash).
 */
export function ProvisioningSplash() {
  const { state } = useApp();
  const [phaseMessage, setPhaseMessage] = React.useState<string>(
    "Starting flowstate…",
  );
  const [error, setError] = React.useState<string | null>(null);
  const [showAfter, setShowAfter] = React.useState(false);

  // Subscribe to `provision` events from the Rust side. We accept that
  // this effect runs in React.StrictMode's double-invoke; the listen
  // cleanup is idempotent.
  React.useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    (async () => {
      try {
        const u = await listen<ProvisionEvent>("provision", ({ payload }) => {
          if (payload.kind === "started") {
            setPhaseMessage(payload.message);
            setError(null);
          } else if (payload.kind === "all-done") {
            setPhaseMessage("Finishing up…");
          } else if (payload.kind === "failed") {
            setError(
              `Failed during ${payload.phase}: ${payload.error.split("\n")[0]}`,
            );
          }
        });
        if (cancelled) {
          u();
        } else {
          unlisten = u;
        }
      } catch {
        // If the Tauri event bridge isn't available (e.g. during
        // hot-reload or non-Tauri context), just leave the default
        // copy in place. The splash will still hide on `ready`.
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  // Hide the splash completely for the first ~300 ms — warm-cache
  // launches go from mount → ready in well under that, and flashing
  // a splash screen on every boot is worse UX than showing nothing
  // for the first frame or two.
  React.useEffect(() => {
    if (state.ready) return; // already ready → nothing to show
    const t = window.setTimeout(() => setShowAfter(true), 300);
    return () => window.clearTimeout(t);
  }, [state.ready]);

  if (state.ready) return null;
  if (!showAfter && !error) return null;

  return (
    <div
      role="status"
      aria-live="polite"
      // z-[9999] so we sit above the sidebar, dock, update banner,
      // and any floating toaster that might render underneath. bg
      // matches `tauri.conf.json`'s window background (#252525) so
      // there's no visible seam while the splash paints.
      className="fixed inset-0 z-[9999] flex items-center justify-center bg-[#252525] text-white"
      data-testid="provisioning-splash"
    >
      <div className="flex max-w-md flex-col items-center gap-5 px-6 text-center">
        <div className="text-2xl font-semibold tracking-tight">flowstate</div>
        <div className="flex items-center gap-3">
          {!error && <Spinner />}
          <div
            className={
              error ? "text-sm text-red-400" : "text-sm text-neutral-300"
            }
          >
            {error ?? phaseMessage}
          </div>
        </div>
        {!error && (
          <div className="text-xs leading-relaxed text-neutral-500">
            First launch can take 30–90 seconds while we install Node.js and
            the provider SDKs. Future launches are instant.
          </div>
        )}
        {error && (
          <div className="text-xs leading-relaxed text-neutral-500">
            Try relaunching the app. If the error persists, the daemon log
            under&nbsp;
            <code className="rounded bg-neutral-800 px-1 py-0.5 text-[10px]">
              ~/Library/Logs/Flowstate
            </code>
            &nbsp;has the full stack.
          </div>
        )}
      </div>
    </div>
  );
}

function Spinner() {
  // Pure-CSS spinner — no dependency on lucide-react to keep this
  // component renderable before the main bundle has fully
  // tree-shaken its icon imports.
  return (
    <div
      className="h-4 w-4 animate-spin rounded-full border-2 border-neutral-600 border-t-white"
      aria-hidden="true"
    />
  );
}
