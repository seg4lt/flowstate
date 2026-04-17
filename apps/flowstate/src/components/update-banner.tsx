// Global update banner. Mounts once at the app root (next to
// <Toaster />) and renders a fixed banner whenever the updater
// store has something interesting to show — a fetched update
// awaiting install, a download in progress, or the post-download
// "installing/restarting" beat. "Idle", "checking", "up-to-date",
// and "error" are silent here so the banner doesn't flash on every
// startup poll; the manual Settings button handles those cases via
// inline toasts.
import { Download, Loader2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { installUpdate, useUpdaterStatus } from "@/lib/updater";

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export function UpdateBanner() {
  const status = useUpdaterStatus();

  if (status.kind === "available") {
    return (
      <div className="pointer-events-auto fixed bottom-4 left-1/2 z-[200] -translate-x-1/2">
        <div className="flex items-center gap-3 rounded-md border border-border bg-background/95 px-4 py-3 shadow-lg backdrop-blur-sm">
          <Download className="h-4 w-4 shrink-0 text-foreground" />
          <div className="text-sm">
            <div className="font-medium">
              Flowstate {status.update.version} is available
            </div>
            <div className="text-xs text-muted-foreground">
              You're on {status.update.currentVersion}. Restart to install.
            </div>
          </div>
          <Button
            size="sm"
            onClick={() => void installUpdate(status.update)}
          >
            Install &amp; Restart
          </Button>
        </div>
      </div>
    );
  }

  if (status.kind === "downloading") {
    const pct =
      status.total != null && status.total > 0
        ? Math.round((status.downloaded / status.total) * 100)
        : null;
    return (
      <div className="pointer-events-auto fixed bottom-4 left-1/2 z-[200] -translate-x-1/2">
        <div className="flex items-center gap-3 rounded-md border border-border bg-background/95 px-4 py-3 shadow-lg backdrop-blur-sm">
          <Loader2 className="h-4 w-4 animate-spin text-foreground" />
          <div className="text-sm">
            Downloading update
            {pct != null
              ? ` — ${pct}%`
              : ` — ${formatBytes(status.downloaded)}`}
          </div>
        </div>
      </div>
    );
  }

  if (status.kind === "installing") {
    return (
      <div className="pointer-events-auto fixed bottom-4 left-1/2 z-[200] -translate-x-1/2">
        <div className="flex items-center gap-3 rounded-md border border-border bg-background/95 px-4 py-3 shadow-lg backdrop-blur-sm">
          <Loader2 className="h-4 w-4 animate-spin text-foreground" />
          <div className="text-sm">Installing update — restarting…</div>
        </div>
      </div>
    );
  }

  return null;
}
