import { useEffect, useState } from "react";

function formatElapsed(startedAtMs: number, nowMs: number): string {
  const elapsedSeconds = Math.max(0, Math.floor((nowMs - startedAtMs) / 1000));
  if (elapsedSeconds < 60) return `${elapsedSeconds}s`;
  const minutes = Math.floor(elapsedSeconds / 60);
  const seconds = elapsedSeconds % 60;
  if (minutes < 60) return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const remMin = minutes % 60;
  return remMin > 0 ? `${hours}h ${remMin}m` : `${hours}h`;
}

export function WorkingIndicator({ startedAt }: { startedAt: string }) {
  const startedAtMs = Date.parse(startedAt);
  const [nowMs, setNowMs] = useState(() => Date.now());

  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);

  const elapsed = Number.isFinite(startedAtMs) ? formatElapsed(startedAtMs, nowMs) : "0s";

  return (
    <div className="flex items-center gap-2 py-1 text-[11px] text-muted-foreground">
      <span className="inline-flex items-center gap-[3px]">
        <span className="h-1 w-1 rounded-full bg-muted-foreground/50 animate-pulse" />
        <span className="h-1 w-1 rounded-full bg-muted-foreground/50 animate-pulse [animation-delay:200ms]" />
        <span className="h-1 w-1 rounded-full bg-muted-foreground/50 animate-pulse [animation-delay:400ms]" />
      </span>
      <span>Working for {elapsed}</span>
    </div>
  );
}
