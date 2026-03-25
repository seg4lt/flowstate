import * as React from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Switch } from "@/components/ui/switch";
import { useApp } from "@/stores/app-store";
import { toast } from "@/hooks/use-toast";
import { getAppDataDir } from "@/lib/api";
import {
  POOL_SIZE_MIN,
  getDefaultPoolSize,
  getMaxPoolSize,
  readPoolSizeSetting,
  writePoolSizeSetting,
} from "@/lib/pierre-diffs-worker";
import {
  readWorktreeBasePath,
  writeWorktreeBasePath,
} from "@/lib/worktree-settings";
import type { ProviderKind, ProviderStatus } from "@/lib/types";

const PROVIDER_COLORS: Record<ProviderKind, string> = {
  claude: "bg-amber-500",
  claude_cli: "bg-purple-500",
  codex: "bg-green-500",
  github_copilot: "bg-blue-500",
  github_copilot_cli: "bg-cyan-500",
};

const PROVIDER_LABELS: Record<ProviderKind, string> = {
  claude: "Claude (SDK)",
  claude_cli: "Claude (CLI)",
  codex: "Codex",
  github_copilot: "GitHub Copilot",
  github_copilot_cli: "GitHub Copilot (CLI)",
};

const PROVIDER_ORDER: ProviderKind[] = [
  "claude",
  "claude_cli",
  "codex",
  "github_copilot",
  "github_copilot_cli",
];

function SettingsGroup({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="mb-8">
      <div className="mb-3">
        <h2 className="text-sm font-semibold">{title}</h2>
        {description && (
          <p className="mt-0.5 text-xs text-muted-foreground">{description}</p>
        )}
      </div>
      <div className="overflow-hidden rounded-lg border border-border bg-card">
        {children}
      </div>
    </section>
  );
}

function ProviderRow({
  kind,
  provider,
  onRefresh,
  refreshing,
  onToggleEnabled,
}: {
  kind: ProviderKind;
  provider: ProviderStatus | undefined;
  onRefresh: () => void;
  refreshing: boolean;
  onToggleEnabled: (enabled: boolean) => void;
}) {
  const label = PROVIDER_LABELS[kind];
  const modelCount = provider?.models.length ?? 0;
  const isReady = provider?.status === "ready";
  // Provider entries from the daemon always carry an `enabled` flag
  // after Phase 2, but during the cold-start bootstrap window we can
  // render before the first `welcome` lands, so default to true.
  const enabled = provider?.enabled ?? true;
  const statusText = !enabled
    ? "Disabled"
    : provider
      ? provider.status === "ready"
        ? `${modelCount} model${modelCount === 1 ? "" : "s"}`
        : (provider.message ?? provider.status)
      : "checking...";

  return (
    <div
      className={`flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0 ${
        !enabled ? "opacity-60" : ""
      }`}
    >
      <span
        className={`inline-block h-2 w-2 shrink-0 rounded-full ${
          isReady && enabled ? PROVIDER_COLORS[kind] : "bg-muted-foreground/30"
        }`}
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-medium">{label}</div>
        <div className="truncate text-xs text-muted-foreground">
          {statusText}
        </div>
      </div>
      <Button
        variant="outline"
        size="sm"
        disabled={!isReady || refreshing || !enabled}
        onClick={onRefresh}
      >
        {refreshing ? <Loader2 className="animate-spin" /> : <RefreshCw />}
        Refresh models
      </Button>
      <Switch
        checked={enabled}
        onCheckedChange={onToggleEnabled}
        aria-label={`${enabled ? "Disable" : "Enable"} ${label}`}
      />
    </div>
  );
}

// Single row in the Performance group. The current value lives in
// flowzen's own SQLite (`user_config` table) — fetched once on
// mount via `readPoolSizeSetting`, written back on every commit
// via `writePoolSizeSetting`. Both calls are async because they
// cross the Tauri IPC bridge, but local SQLite reads are
// sub-millisecond so the row feels instant in practice. The pool
// itself isn't rebuilt — main.tsx reads the value once at app boot
// and the @pierre/diffs pool is a singleton — so the hint text
// tells the user to restart for the change to take effect.
function PoolSizeRow() {
  const maxPoolSize = React.useMemo(() => getMaxPoolSize(), []);
  const cores = React.useMemo(
    () =>
      (typeof navigator !== "undefined" && navigator.hardwareConcurrency) || 4,
    [],
  );
  // `null` = still loading the persisted value from sqlite; once
  // it resolves we swap to a number and never go back.
  const [value, setValue] = React.useState<number | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    readPoolSizeSetting()
      .then((resolved) => {
        if (cancelled) return;
        setValue(resolved);
      })
      .catch(() => {
        if (cancelled) return;
        setValue(getDefaultPoolSize());
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const commit = React.useCallback(
    (next: number) => {
      const clamped = Math.max(
        POOL_SIZE_MIN,
        Math.min(maxPoolSize, Math.round(next)),
      );
      setValue(clamped);
      void writePoolSizeSetting(clamped);
    },
    [maxPoolSize],
  );

  return (
    <div className="flex items-start gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Highlighter workers</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Background workers for syntax highlighting in the diff panel and
          code view. More workers tokenize files in parallel — faster on big
          diffs, more idle memory.
        </div>
        <div className="mt-1 text-[11px] text-muted-foreground">
          Detected {cores} {cores === 1 ? "core" : "cores"} · range{" "}
          {POOL_SIZE_MIN}–{maxPoolSize} · default {getDefaultPoolSize()} ·
          restart Flowzen to apply.
        </div>
      </div>
      <input
        type="number"
        min={POOL_SIZE_MIN}
        max={maxPoolSize}
        step={1}
        value={value ?? ""}
        disabled={value === null}
        onChange={(e) => {
          const parsed = Number.parseInt(e.target.value, 10);
          if (Number.isFinite(parsed)) commit(parsed);
        }}
        className="h-8 w-16 rounded-md border border-input bg-background px-2 text-sm disabled:opacity-50"
        aria-label="Highlighter worker pool size"
      />
    </div>
  );
}

// Configurable base directory under which new git worktrees are
// created. Persisted to `user_config.sqlite` via the kv table. When
// empty, the branch-switcher's create-worktree flow falls back to
// `<parent-dir-of-project>/worktrees/...` — a sibling folder next
// to the repo. When set, that prefix is used instead so users can
// keep all their worktrees under a dedicated workspace dir like
// `~/Code/worktrees`.
function WorktreeBasePathRow() {
  // `null` before the sqlite read resolves, `""` after if the user
  // hasn't set anything (renders as placeholder), otherwise the raw
  // path string.
  const [value, setValue] = React.useState<string | null>(null);
  const [saved, setSaved] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    readWorktreeBasePath()
      .then((resolved) => {
        if (cancelled) return;
        setValue(resolved ?? "");
        setSaved(resolved ?? "");
      })
      .catch(() => {
        if (cancelled) return;
        setValue("");
        setSaved("");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const commit = React.useCallback(async () => {
    if (value === null) return;
    const trimmed = value.trim();
    if (trimmed === saved) return;
    await writeWorktreeBasePath(trimmed);
    setSaved(trimmed);
    toast({
      description:
        trimmed.length > 0
          ? `Worktree base set to ${trimmed}`
          : "Worktree base reset to default",
      duration: 2000,
    });
  }, [saved, value]);

  return (
    <div className="flex items-start gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Worktree base directory</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Parent folder for new git worktrees created from the branch
          switcher. Leave blank to use a sibling of each project (the
          default). Each worktree lands under{" "}
          <span className="font-mono">
            &lt;base&gt;/&lt;project&gt;-worktrees/&lt;project&gt;-&lt;name&gt;
          </span>
          .
        </div>
        <div className="mt-2">
          <input
            type="text"
            value={value ?? ""}
            disabled={value === null}
            onChange={(e) => setValue(e.target.value)}
            onBlur={() => void commit()}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void commit();
              }
            }}
            placeholder="/Users/me/Code/worktrees"
            className="w-full rounded-md border border-input bg-background px-2 py-1 font-mono text-[11px] text-foreground disabled:opacity-50"
            aria-label="Worktree base directory"
          />
        </div>
      </div>
    </div>
  );
}

// Read-only display of the cross-platform app data directory
// (where the daemon database, threads dir, and user_config sqlite
// all live). Resolved by the rust side via Tauri's path resolver
// so we get the right OS-specific location:
//   - macOS:   ~/Library/Application Support/<bundle.id>/
//   - Linux:   ~/.local/share/<bundle.id>/
//   - Windows: %APPDATA%/<bundle.id>/
// Click the input to select all, then Cmd/Ctrl+C to copy.
function AppDataDirRow() {
  const [path, setPath] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    getAppDataDir()
      .then((resolved) => {
        if (cancelled) return;
        setPath(resolved);
      })
      .catch((err) => {
        if (cancelled) return;
        setError(String(err));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="flex items-start gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">App data folder</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Where Flowzen keeps its databases, sessions, and config on disk.
          Click the field to select, then Cmd/Ctrl+C to copy.
        </div>
        <div className="mt-2">
          {error ? (
            <div className="text-[11px] text-destructive">{error}</div>
          ) : (
            <input
              type="text"
              readOnly
              value={path ?? "Loading…"}
              onFocus={(e) => e.currentTarget.select()}
              onClick={(e) => e.currentTarget.select()}
              className="w-full rounded-md border border-input bg-muted/30 px-2 py-1 font-mono text-[11px] text-foreground"
              aria-label="App data folder path"
            />
          )}
        </div>
      </div>
    </div>
  );
}

export function SettingsView() {
  const { state, send } = useApp();
  const [refreshingKind, setRefreshingKind] = React.useState<ProviderKind | null>(
    null,
  );

  const providerMap = React.useMemo(
    () => new Map(state.providers.map((p) => [p.kind, p])),
    [state.providers],
  );

  // Detect when a refresh completes: ProviderModelsUpdated flips the models
  // array reference, and the reducer replaces `providers[kind]`. When the
  // entry for the kind we're refreshing changes, clear the spinner.
  const refreshTargetRef = React.useRef<ProviderStatus | undefined>(undefined);
  React.useEffect(() => {
    if (!refreshingKind) {
      refreshTargetRef.current = undefined;
      return;
    }
    const current = providerMap.get(refreshingKind);
    if (refreshTargetRef.current === undefined) {
      refreshTargetRef.current = current;
      return;
    }
    if (current && current !== refreshTargetRef.current) {
      setRefreshingKind(null);
      toast({
        description: `Refreshed ${PROVIDER_LABELS[refreshingKind]} models (${current.models.length} available)`,
        duration: 2000,
      });
    }
  }, [providerMap, refreshingKind]);

  async function handleRefresh(kind: ProviderKind) {
    setRefreshingKind(kind);
    try {
      await send({ type: "refresh_models", provider: kind });
    } catch (err) {
      setRefreshingKind(null);
      toast({
        description: `Failed to refresh ${PROVIDER_LABELS[kind]}: ${(err as Error).message}`,
        duration: 4000,
      });
    }
  }

  async function handleToggleEnabled(kind: ProviderKind, enabled: boolean) {
    try {
      await send({ type: "set_provider_enabled", provider: kind, enabled });
      toast({
        description: `${PROVIDER_LABELS[kind]} ${enabled ? "enabled" : "disabled"}`,
        duration: 2000,
      });
    } catch (err) {
      toast({
        description: `Failed to update ${PROVIDER_LABELS[kind]}: ${(err as Error).message}`,
        duration: 4000,
      });
    }
  }

  return (
    <div className="flex h-svh flex-col">
      <header className="flex h-12 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <span className="font-medium">Settings</span>
      </header>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl px-6 py-8">
          <SettingsGroup
            title="Providers"
            description="Toggle which providers are available, and refresh their cached model lists. Models are cached for 24 hours by default."
          >
            {PROVIDER_ORDER.map((kind) => (
              <ProviderRow
                key={kind}
                kind={kind}
                provider={providerMap.get(kind)}
                onRefresh={() => handleRefresh(kind)}
                refreshing={refreshingKind === kind}
                onToggleEnabled={(enabled) => handleToggleEnabled(kind, enabled)}
              />
            ))}
          </SettingsGroup>
          <SettingsGroup
            title="Performance"
            description="Tune how Flowzen uses your machine's resources."
          >
            <PoolSizeRow />
          </SettingsGroup>
          <SettingsGroup
            title="Git worktrees"
            description="Controls for where new git worktrees land on disk."
          >
            <WorktreeBasePathRow />
          </SettingsGroup>
          <SettingsGroup
            title="Storage"
            description="Where Flowzen keeps its data on disk."
          >
            <AppDataDirRow />
          </SettingsGroup>
        </div>
      </div>
    </div>
  );
}
