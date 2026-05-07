import * as React from "react";
import { cn } from "@/lib/utils";
import { PanelDragHandle } from "@/components/ui/panel-drag-handle";
import { DiffPanel, type DiffStyle } from "./diff-panel";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import type { AggregatedFileDiff } from "@/lib/git-diff-stream";

// Diff-panel sizing. Clamped so neither the chat column nor the diff
// pane can collapse to nothing when the user drags the handle.
export const DIFF_WIDTH_KEY = "flowstate:diff-width";
export const DIFF_STYLE_KEY = "flowstate:diff-style";
export const DIFF_MIN_WIDTH = 360;
const DIFF_DEFAULT_WIDTH = 560;

function readInitialDiffWidth(): number {
  try {
    const saved = window.localStorage.getItem(DIFF_WIDTH_KEY);
    if (saved) {
      const parsed = Number.parseInt(saved, 10);
      if (Number.isFinite(parsed) && parsed >= DIFF_MIN_WIDTH) {
        return parsed;
      }
    }
  } catch {
    /* storage may be unavailable */
  }
  return DIFF_DEFAULT_WIDTH;
}

function readInitialDiffStyle(): DiffStyle {
  try {
    const saved = window.localStorage.getItem(DIFF_STYLE_KEY);
    if (saved === "split" || saved === "unified") return saved;
  } catch {
    /* storage may be unavailable */
  }
  return "split";
}

export interface DiffPanelHostApi {
  /** Whether the diff panel is currently open. */
  open: boolean;
  /** Current streamed diff list — exposed so HeaderActions can render
   *  the `+N/-M` badge on the Diff button without its own subscription. */
  diffs: AggregatedFileDiff[];
  /** Open the panel. Idempotent; activates the streamed subscription
   *  and triggers a forced refresh so newly-landed changes show up. */
  toggle: () => void;
  /** Close the panel. Also drops fullscreen so the split-right slot is
   *  clean for whatever opens next. */
  close: () => void;
  /** Arm the subscription without opening the panel — used on Diff
   *  button hover so the badge hydrates before click. */
  activate: () => void;
  /** Force a refetch of the subscription (new tick). Debounced to
   *  one refresh per 400ms unless `force` is set — branch checkout
   *  passes `force: true`. */
  refresh: (opts?: { force?: boolean }) => void;
}

export interface DiffPanelHostProps {
  sessionId: string;
  projectPath: string | null;
  containerRef: React.RefObject<HTMLDivElement | null>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  fullscreen: boolean;
  onFullscreenChange: (fullscreen: boolean) => void;
}

/** Renders the right-docked diff pane (including its resize handle)
 *  and exposes refresh / activate handlers back up to ChatView via
 *  the forwarded `api` ref. Keeps all width/style/diff-stream state
 *  self-contained so ChatView only threads in-focus booleans. */
export const DiffPanelHost = React.forwardRef<
  DiffPanelHostApi,
  DiffPanelHostProps
>(function DiffPanelHost(props, apiRef) {
  const {
    sessionId,
    projectPath,
    containerRef,
    open,
    onOpenChange,
    fullscreen,
    onFullscreenChange,
  } = props;

  // `diffRefreshTick` bumps to restart the streamed subscription
  // without blowing away the previously-committed file list, so the
  // Diff button badge stays steady across refreshes.
  const [diffRefreshTick, setDiffRefreshTick] = React.useState(0);
  // Latches true the first time the user opens or hovers the diff
  // panel button for this chat view. Gates the streamed subscription
  // itself (`enabled`) AND the stream-event refresh path — before the
  // first interaction we don't run a single git subprocess for this
  // view.
  const [diffSubscriptionActive, setDiffSubscriptionActive] =
    React.useState(false);
  // 400ms grace window to collapse back-to-back `refreshDiffs()`
  // triggers into a single tick bump.
  const lastRefreshAtRef = React.useRef(0);
  const refreshDiffs = React.useCallback((opts?: { force?: boolean }) => {
    const now = Date.now();
    if (!opts?.force && now - lastRefreshAtRef.current < 400) return;
    lastRefreshAtRef.current = now;
    setDiffRefreshTick((t) => t + 1);
  }, []);
  const activateDiffSubscription = React.useCallback(() => {
    setDiffSubscriptionActive(true);
  }, []);

  const [diffWidth, setDiffWidth] = React.useState<number>(readInitialDiffWidth);
  const [diffStyle, setDiffStyleState] =
    React.useState<DiffStyle>(readInitialDiffStyle);
  const setDiffStyle = React.useCallback((s: DiffStyle) => {
    setDiffStyleState(s);
    try {
      window.localStorage.setItem(DIFF_STYLE_KEY, s);
    } catch {
      /* storage may be unavailable */
    }
  }, []);

  const diffStream = useStreamedGitDiffSummary(
    projectPath,
    diffRefreshTick,
    diffSubscriptionActive,
  );
  const diffs = diffStream.diffs;

  const toggle = React.useCallback(() => {
    if (!open) {
      activateDiffSubscription();
      refreshDiffs({ force: true });
      onOpenChange(true);
    } else {
      onFullscreenChange(false);
      onOpenChange(false);
    }
  }, [open, onOpenChange, onFullscreenChange, activateDiffSubscription, refreshDiffs]);

  const close = React.useCallback(() => {
    onFullscreenChange(false);
    onOpenChange(false);
  }, [onOpenChange, onFullscreenChange]);

  // Publish the imperative API so ChatView can call refresh/activate
  // without threading every handler as a prop.
  React.useImperativeHandle(
    apiRef,
    () => ({
      open,
      diffs,
      toggle,
      close,
      activate: activateDiffSubscription,
      refresh: refreshDiffs,
    }),
    [open, diffs, toggle, close, activateDiffSubscription, refreshDiffs],
  );

  if (!open) return null;

  return (
    <>
      {!fullscreen && (
        <PanelDragHandle
          containerRef={containerRef}
          width={diffWidth}
          onResize={setDiffWidth}
          storageKey={DIFF_WIDTH_KEY}
          minWidth={DIFF_MIN_WIDTH}
          ariaLabel="Resize diff panel"
        />
      )}
      <aside
        className={cn(
          "flex min-h-0 min-w-0 flex-col overflow-hidden border-l border-border bg-background",
          fullscreen ? "flex-1" : "shrink-0",
        )}
        style={fullscreen ? undefined : { width: diffWidth }}
      >
        <DiffPanel
          projectPath={projectPath}
          sessionId={sessionId}
          diffs={diffs}
          refreshKey={diffRefreshTick}
          streamStatus={diffStream.status}
          style={diffStyle}
          onStyleChange={setDiffStyle}
          onClose={close}
          isFullscreen={fullscreen}
          onToggleFullscreen={() => onFullscreenChange(!fullscreen)}
        />
      </aside>
    </>
  );
});
