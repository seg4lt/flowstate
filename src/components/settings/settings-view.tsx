import * as React from "react";
import { Loader2, Monitor, Moon, RefreshCw, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Switch } from "@/components/ui/switch";
import { useApp } from "@/stores/app-store";
import { useTheme, type ThemePreference } from "@/hooks/use-theme";
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
import {
  readDefaultEffort,
  writeDefaultEffort,
  readDefaultPermissionMode,
  writeDefaultPermissionMode,
  readDefaultModel,
  writeDefaultModel,
} from "@/lib/defaults-settings";
import { useContextDisplaySetting } from "@/hooks/use-context-display-setting";
import { EFFORT_OPTIONS } from "@/components/chat/effort-selector";
import { MODE_ORDER, MODE_LABELS } from "@/lib/mode-cycling";
import type {
  PermissionMode,
  ProviderKind,
  ProviderStatus,
  ReasoningEffort,
} from "@/lib/types";

const PROVIDER_COLORS: Record<ProviderKind, string> = {
  claude: "bg-amber-500",
  claude_cli: "bg-purple-500",
  codex: "bg-green-500",
  github_copilot: "bg-blue-500",
  github_copilot_cli: "bg-cyan-500",
};

const PROVIDER_LABELS: Record<ProviderKind, string> = {
  claude: "Claude",
  claude_cli: "Claude 2",
  codex: "Codex",
  github_copilot: "GitHub Copilot",
  github_copilot_cli: "GitHub Copilot 2",
};

const PROVIDER_ORDER: ProviderKind[] = [
  "claude",
  "claude_cli",
  "codex",
  "github_copilot",
  "github_copilot_cli",
];

const THEME_OPTIONS: {
  value: ThemePreference;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
}[] = [
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
  { value: "system", label: "System", icon: Monitor },
];

function ThemeRow() {
  const { preference, setTheme } = useTheme();

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Theme</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Choose light, dark, or sync with your operating system.
        </div>
      </div>
      <div className="flex gap-1">
        {THEME_OPTIONS.map((opt) => (
          <Button
            key={opt.value}
            variant={preference === opt.value ? "default" : "outline"}
            size="sm"
            onClick={() => setTheme(opt.value)}
          >
            <opt.icon className="h-4 w-4" />
            {opt.label}
          </Button>
        ))}
      </div>
    </div>
  );
}

function ContextDisplayRow() {
  const { showContextDisplay, setShowContextDisplay } =
    useContextDisplaySetting();

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Show context size</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Display token usage and context window size in the chat toolbar.
        </div>
      </div>
      <Switch
        checked={showContextDisplay}
        onCheckedChange={setShowContextDisplay}
        aria-label="Show context size display"
      />
    </div>
  );
}

function DefaultEffortRow() {
  const [value, setValue] = React.useState<ReasoningEffort>("high");

  React.useEffect(() => {
    let cancelled = false;
    readDefaultEffort().then((saved) => {
      if (!cancelled && saved) setValue(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  function handleChange(next: ReasoningEffort) {
    setValue(next);
    void writeDefaultEffort(next);
  }

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Default effort</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Reasoning effort level applied to new turns.
        </div>
      </div>
      <select
        value={value}
        onChange={(e) => handleChange(e.target.value as ReasoningEffort)}
        className="h-8 rounded-md border border-input bg-background px-2 text-sm"
        aria-label="Default effort"
      >
        {EFFORT_OPTIONS.map((opt) => (
          <option key={opt.value} value={opt.value}>
            {opt.label}
          </option>
        ))}
      </select>
    </div>
  );
}

function DefaultPermissionModeRow() {
  const [value, setValue] = React.useState<PermissionMode>("accept_edits");

  React.useEffect(() => {
    let cancelled = false;
    readDefaultPermissionMode().then((saved) => {
      if (!cancelled && saved) setValue(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  function handleChange(next: PermissionMode) {
    setValue(next);
    void writeDefaultPermissionMode(next);
  }

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Default permission mode</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Permission mode applied to new sessions.
        </div>
      </div>
      <select
        value={value}
        onChange={(e) => handleChange(e.target.value as PermissionMode)}
        className="h-8 rounded-md border border-input bg-background px-2 text-sm"
        aria-label="Default permission mode"
      >
        {MODE_ORDER.map((mode) => (
          <option key={mode} value={mode}>
            {MODE_LABELS[mode]}
          </option>
        ))}
      </select>
    </div>
  );
}

function DefaultModelRow() {
  const { state } = useApp();
  const [defaults, setDefaults] = React.useState<
    Record<ProviderKind, string | null>
  >({
    claude: null,
    claude_cli: null,
    codex: null,
    github_copilot: null,
    github_copilot_cli: null,
  });
  const [loaded, setLoaded] = React.useState(false);

  // Read saved defaults for all providers on mount.
  React.useEffect(() => {
    let cancelled = false;
    Promise.all(
      PROVIDER_ORDER.map(async (kind) => {
        const model = await readDefaultModel(kind);
        return [kind, model] as const;
      }),
    ).then((entries) => {
      if (cancelled) return;
      const next: Record<string, string | null> = {};
      for (const [kind, model] of entries) {
        next[kind] = model;
      }
      setDefaults(next as Record<ProviderKind, string | null>);
      setLoaded(true);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  // Only show providers that are ready and have models.
  const readyProviders = React.useMemo(
    () =>
      PROVIDER_ORDER.map((kind) => ({
        kind,
        provider: state.providers.find((p) => p.kind === kind),
      })).filter(
        (entry) =>
          entry.provider &&
          entry.provider.enabled &&
          entry.provider.status === "ready" &&
          entry.provider.models.length > 0,
      ),
    [state.providers],
  );

  function handleChange(kind: ProviderKind, model: string) {
    // Empty string means "use first / no preference"
    const resolved = model === "" ? null : model;
    setDefaults((prev) => ({ ...prev, [kind]: resolved }));
    if (resolved) {
      void writeDefaultModel(kind, resolved);
    } else {
      // Write empty string to clear the default
      void writeDefaultModel(kind, "");
    }
  }

  if (!loaded) return null;

  return (
    <div className="border-b border-border px-4 py-3 last:border-b-0">
      <div className="mb-3">
        <div className="text-sm font-medium">Default model</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Preferred model per provider. New sessions auto-select this model when
          available.
        </div>
      </div>
      {readyProviders.length === 0 ? (
        <div className="text-xs text-muted-foreground">
          No providers with models available.
        </div>
      ) : (
        <div className="space-y-2">
          {readyProviders.map(({ kind, provider }) => {
            const models = provider!.models;
            const current = defaults[kind];
            return (
              <div
                key={kind}
                className="flex items-center gap-3"
              >
                <span
                  className={`inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[kind]}`}
                />
                <span className="min-w-0 flex-1 truncate text-xs font-medium">
                  {PROVIDER_LABELS[kind]}
                </span>
                <select
                  value={current ?? ""}
                  onChange={(e) => handleChange(kind, e.target.value)}
                  className="h-7 max-w-48 truncate rounded-md border border-input bg-background px-2 text-xs"
                  aria-label={`Default model for ${PROVIDER_LABELS[kind]}`}
                >
                  <option value="">Use first available</option>
                  {models.map((m) => (
                    <option key={m.value} value={m.value}>
                      {m.label}
                    </option>
                  ))}
                </select>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

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
// flowstate's own SQLite (`user_config` table) — fetched once on
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
          restart Flowstate to apply.
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
          Where Flowstate keeps its databases, sessions, and config on disk.
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
            title="Appearance"
            description="Customize how Flowstate looks."
          >
            <ThemeRow />
            <ContextDisplayRow />
          </SettingsGroup>
          <SettingsGroup
            title="Defaults"
            description="Default values for new sessions. These apply across all providers."
          >
            <DefaultEffortRow />
            <DefaultPermissionModeRow />
            <DefaultModelRow />
          </SettingsGroup>
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
            description="Tune how Flowstate uses your machine's resources."
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
            description="Where Flowstate keeps its data on disk."
          >
            <AppDataDirRow />
          </SettingsGroup>
        </div>
      </div>
    </div>
  );
}
