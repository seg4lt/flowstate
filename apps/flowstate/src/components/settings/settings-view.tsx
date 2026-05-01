import * as React from "react";
import {
  AlertCircle,
  ArrowUpCircle,
  ChevronDown,
  ChevronUp,
  FolderOpen,
  Loader2,
  Monitor,
  Moon,
  Plus,
  RefreshCw,
  Sun,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { isMacOS } from "@/lib/popout";
import { Switch } from "@/components/ui/switch";
import { useApp, useProvisionFailures } from "@/stores/app-store";
import { useTheme, type ThemePreference } from "@/hooks/use-theme";
import { toast } from "@/hooks/use-toast";
import {
  clearRuntimeCache,
  getAppDataDir,
  getCacheDir,
  getCaffeinateStatus,
  getLogDir,
  installCli,
  installCliStatus,
  killCaffeinate,
  listBinarySearchPaths,
  refreshBinarySearchPaths,
  refreshCaffeinate,
  retryProvisionPhase,
  type CaffeinateStatus,
  type InstallCliStatus,
  type InstallCliTarget,
} from "@/lib/api";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { relaunch } from "@tauri-apps/plugin-process";
import { confirm } from "@tauri-apps/plugin-dialog";
import { Trash2 } from "lucide-react";
import {
  checkForUpdate,
  resetUpdaterStatus,
  useUpdaterStatus,
} from "@/lib/updater";
import { getVersion } from "@tauri-apps/api/app";
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
import { McpServersPanel } from "@/components/settings/mcp-servers-panel";
import {
  readDefaultEffort,
  writeDefaultEffort,
  readDefaultPermissionMode,
  writeDefaultPermissionMode,
  readDefaultModel,
  writeDefaultModel,
  readDefaultProvider,
  writeDefaultProvider,
  readStrictPlanMode,
  writeStrictPlanMode,
  readCaffeinate,
  writeCaffeinate,
  readBinarySearchPaths,
  writeBinarySearchPaths,
  DEFAULT_PROVIDER,
} from "@/lib/defaults-settings";
import { PLAN_MODE_MUTATING_TOOLS_LABEL } from "@/lib/tool-policy";
import { ShortcutsDialog } from "@/lib/keyboard";
import { useContextDisplaySetting } from "@/hooks/use-context-display-setting";
import { useEditorPrefs } from "@/hooks/use-editor-prefs";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { useCheckpointSettings } from "@/hooks/useCheckpointSettings";
import { visibleEffortOptions } from "@/components/chat/effort-selector";
import { resolveModelDisplay } from "@/lib/model-lookup";
import { MODE_ORDER, MODE_LABELS } from "@/lib/mode-cycling";
import type {
  PermissionMode,
  ProviderKind,
  ProviderStatus,
  ReasoningEffort,
} from "@/lib/types";

import {
  PROVIDER_COLORS,
  PROVIDER_KINDS as PROVIDER_ORDER,
  PROVIDER_META,
} from "@/lib/providers";

const PROVIDER_LABELS: Record<ProviderKind, string> = Object.fromEntries(
  PROVIDER_ORDER.map((k) => [k, PROVIDER_META[k].label]),
) as Record<ProviderKind, string>;

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

// Vim mode is an app-wide preference (one boolean per user, persisted
// in localStorage and broadcast through `useEditorPrefs`'s module
// singleton). It used to live as a toolbar button in the code view,
// but since flipping it always affected every editor anyway, the
// toggle now lives here in Settings — one source of truth, no
// per-pane chrome to maintain. `useEditorPrefs()` with no sessionId
// is correct: the optional argument is only used for the per-session
// gitMode flag, which we don't read in this row.
function VimModeRow() {
  const { vimEnabled, setVimEnabled } = useEditorPrefs();

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Vim mode</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Enable vim keybindings in the code viewer.
        </div>
      </div>
      <Switch
        checked={vimEnabled}
        onCheckedChange={setVimEnabled}
        aria-label="Toggle vim mode"
      />
    </div>
  );
}

function DefaultProviderRow() {
  const { isProviderEnabled } = useProviderEnabled();
  const [value, setValue] = React.useState<ProviderKind>(DEFAULT_PROVIDER);

  React.useEffect(() => {
    let cancelled = false;
    readDefaultProvider().then((saved) => {
      if (!cancelled && saved) setValue(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  // Show only enabled providers (ready or not — the user may enable a
  // provider that hasn't finished its health check yet).
  const enabledProviders = React.useMemo(
    () => PROVIDER_ORDER.filter((kind) => isProviderEnabled(kind)),
    [isProviderEnabled],
  );

  function handleChange(next: ProviderKind) {
    setValue(next);
    void writeDefaultProvider(next);
  }

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Default provider</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Provider used when opening new threads — for example when creating
          a worktree or starting a session without explicitly picking one.
        </div>
      </div>
      <select
        value={value}
        onChange={(e) => handleChange(e.target.value as ProviderKind)}
        className="h-8 rounded-md border border-input bg-background px-2 text-sm"
        aria-label="Default provider"
      >
        {enabledProviders.map((kind) => (
          <option key={kind} value={kind}>
            {PROVIDER_LABELS[kind]}
          </option>
        ))}
      </select>
    </div>
  );
}

function DefaultEffortRow() {
  const { state } = useApp();
  const [value, setValue] = React.useState<ReasoningEffort>("high");
  // Track the saved default provider + its default model so we can
  // gate the dropdown to levels that provider/model actually accepts.
  // `xhigh` / `max` are Opus-4.7-only today, so users on a Sonnet
  // default shouldn't see them in the global picker.
  const [defaultProvider, setDefaultProvider] =
    React.useState<ProviderKind>(DEFAULT_PROVIDER);
  const [defaultModel, setDefaultModel] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    readDefaultEffort().then((saved) => {
      if (!cancelled && saved) setValue(saved);
    });
    readDefaultProvider().then((saved) => {
      if (!cancelled) setDefaultProvider(saved ?? DEFAULT_PROVIDER);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  React.useEffect(() => {
    let cancelled = false;
    readDefaultModel(defaultProvider).then((m) => {
      if (!cancelled) setDefaultModel(m);
    });
    return () => {
      cancelled = true;
    };
  }, [defaultProvider]);

  // Resolve the saved default model against the live provider
  // catalog so we can read its `supportedEffortLevels`. Falls through
  // to an empty list when no default model is saved yet or the
  // provider catalog hasn't hydrated — `visibleEffortOptions`
  // interprets that as "hide `xhigh` / `max`", which is the safe
  // baseline.
  const supportedEffortLevels = defaultModel
    ? (resolveModelDisplay(defaultModel, defaultProvider, state.providers).entry
        ?.supportedEffortLevels ?? [])
    : [];
  const options = visibleEffortOptions(supportedEffortLevels);

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
        {options.map((opt) => (
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

function StrictPlanModeRow() {
  const [enabled, setEnabled] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    readStrictPlanMode().then((saved) => {
      if (!cancelled) setEnabled(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  function handleChange(next: boolean) {
    setEnabled(next);
    void writeStrictPlanMode(next);
  }

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Strict plan mode</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Auto-deny {PLAN_MODE_MUTATING_TOOLS_LABEL} while in plan mode, so an
          accidental Allow click can't exit plan mode early. The baseline
          plan-mode gate still prompts without this.
        </div>
      </div>
      <Switch
        checked={enabled}
        onCheckedChange={handleChange}
        aria-label="Strict plan mode"
      />
    </div>
  );
}

/**
 * Probe whether the macOS caffeinate Tauri command is registered.
 * The command is only registered when `cfg!(target_os = "macos")`,
 * so an `invoke` failure here is the cleanest cross-platform signal
 * — no extra `@tauri-apps/plugin-os` dep needed. Resolves to `true`
 * once the probe succeeds, `false` once it fails, `null` while
 * still in flight.
 */
function useCaffeinateSupport(): boolean | null {
  const [supported, setSupported] = React.useState<boolean | null>(null);
  React.useEffect(() => {
    let cancelled = false;
    getCaffeinateStatus()
      .then(() => {
        if (!cancelled) setSupported(true);
      })
      .catch(() => {
        if (!cancelled) setSupported(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);
  return supported;
}

function CaffeinateRow() {
  const [enabled, setEnabled] = React.useState(false);
  const [status, setStatus] = React.useState<CaffeinateStatus | null>(null);
  const [killing, setKilling] = React.useState(false);

  // Initial load: read the persisted toggle from user_config.
  React.useEffect(() => {
    let cancelled = false;
    readCaffeinate().then((saved) => {
      if (!cancelled) setEnabled(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  // Status polling. Two seconds is fast enough to catch the
  // "caffeinate just respawned after timeout" transition without
  // making a meaningful CPU dent. The poll is paused while the tab
  // is hidden — `setInterval` keeps firing but each invoke that
  // fails (e.g. webview hidden) just keeps the previous status.
  React.useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      try {
        const s = await getCaffeinateStatus();
        if (!cancelled) setStatus(s);
      } catch {
        /* command may not be available; useCaffeinateSupport handles that */
      }
    };
    void refresh();
    const id = window.setInterval(() => void refresh(), 2000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  async function handleToggle(next: boolean) {
    setEnabled(next);
    await writeCaffeinate(next);
    // Tell the controller to act on the new value immediately
    // rather than wait for the next turn boundary.
    try {
      await refreshCaffeinate();
    } catch {
      /* non-macOS — shouldn't happen since the row is gated, but ignore */
    }
    try {
      setStatus(await getCaffeinateStatus());
    } catch {
      /* leave previous status */
    }
  }

  async function handleKill() {
    if (killing) return;
    setKilling(true);
    try {
      await killCaffeinate();
      setStatus(await getCaffeinateStatus());
    } catch (err) {
      toast({
        description: `Force-kill failed: ${String(err)}`,
        duration: 3000,
      });
    } finally {
      setKilling(false);
    }
  }

  const running = !!status?.running;

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Prevent display sleep</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Run <code className="font-mono text-[11px]">caffeinate -d</code>{" "}
          while a request is in flight, so your display stays on (and you
          aren't auto-logged out) during long agent turns. Re-spawned
          automatically on a timer for crash safety.
          {running && (
            <span className="ml-1 text-emerald-600 dark:text-emerald-400">
              Active{status?.pid ? ` (PID ${status.pid})` : ""}.
            </span>
          )}
        </div>
      </div>
      <Button
        variant="outline"
        size="sm"
        onClick={() => void handleKill()}
        disabled={!running || killing}
        aria-label="Force kill caffeinate"
      >
        {killing ? <Loader2 className="animate-spin" /> : null}
        Force kill
      </Button>
      <Switch
        checked={enabled}
        onCheckedChange={(v) => void handleToggle(v)}
        aria-label="Prevent display sleep while a request is in flight"
      />
    </div>
  );
}

/**
 * Editable list of extra directories the binary resolver should search
 * when locating provider CLIs (`claude`, `codex`, `copilot`, ...).
 * The resolver consults these right after the PATH walk and before
 * the curated platform fallbacks, so it's the explicit escape hatch
 * for "I have it installed but Flowstate can't find it" — common on
 * Windows where Tauri GUI launches inherit a much narrower PATH than
 * the user's PowerShell.
 *
 * Storage: `binaries.search_paths` user_config key, JSON-encoded
 * array of strings. Writes go through `writeBinarySearchPaths` then
 * `refreshBinarySearchPaths` so the in-process resolver picks up
 * the change immediately, no daemon restart.
 */
function BinarySearchPathsRow() {
  const [paths, setPaths] = React.useState<string[]>([]);
  const [draft, setDraft] = React.useState("");
  const [active, setActive] = React.useState<string[]>([]);
  const [loading, setLoading] = React.useState(true);
  const [saving, setSaving] = React.useState(false);

  // Initial load: pull both the persisted list (source of truth) and
  // the daemon's currently-applied snapshot. They should match —
  // showing both lets the user spot a stale config (e.g. config
  // edited externally while the daemon was running).
  React.useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [persisted, applied] = await Promise.all([
          readBinarySearchPaths(),
          listBinarySearchPaths().catch(() => [] as string[]),
        ]);
        if (cancelled) return;
        setPaths(persisted);
        setActive(applied);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Single helper — every mutation goes through here so we never
  // forget to `refreshBinarySearchPaths` after `writeBinarySearchPaths`.
  // Keeps persisted + applied in sync.
  async function commit(next: string[]) {
    setSaving(true);
    setPaths(next);
    try {
      await writeBinarySearchPaths(next);
      await refreshBinarySearchPaths();
      try {
        setActive(await listBinarySearchPaths());
      } catch {
        /* leave previous active list */
      }
    } catch (err) {
      toast({
        description: `Failed to save search paths: ${String(err)}`,
        duration: 3000,
      });
    } finally {
      setSaving(false);
    }
  }

  function handleAdd() {
    const trimmed = draft.trim();
    if (!trimmed) return;
    if (paths.includes(trimmed)) {
      // Don't dedupe silently — visible feedback prevents the user
      // wondering why the click "did nothing". 1.5s is enough to
      // read without lingering.
      toast({ description: "Already in the list", duration: 1500 });
      setDraft("");
      return;
    }
    setDraft("");
    void commit([...paths, trimmed]);
  }

  function handleRemove(idx: number) {
    void commit(paths.filter((_, i) => i !== idx));
  }

  // Lightweight drift indicator — when the user edits the file
  // externally OR a previous write half-applied, this calls it out
  // so they aren't debugging silent mismatches.
  const drifted =
    !loading &&
    (active.length !== paths.length ||
      active.some((p, i) => p !== paths[i]));

  return (
    <div className="border-b border-border px-4 py-3 last:border-b-0">
      <div className="text-sm font-medium">Extra binary search paths</div>
      <div className="mt-0.5 text-xs text-muted-foreground">
        Directories the resolver checks after PATH and before the built-in
        fallbacks. Useful when a provider CLI is installed somewhere
        Flowstate can't find on its own — particularly on Windows where
        GUI launches inherit a stripped PATH compared to your shell.
      </div>

      {/* Existing entries. Each row shows the path + a small Remove
          button. We deliberately don't allow inline editing — every
          path here is a directory on disk, and re-typing it is
          cheaper than a buggy edit-in-place. */}
      {paths.length > 0 && (
        <ul className="mt-3 space-y-1.5">
          {paths.map((p, idx) => (
            <li
              key={`${idx}-${p}`}
              className="flex items-center gap-2 rounded-md border border-border bg-muted/30 px-2 py-1"
            >
              <code className="flex-1 truncate font-mono text-[12px]">{p}</code>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => handleRemove(idx)}
                disabled={saving}
                aria-label={`Remove ${p}`}
                title="Remove"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </Button>
            </li>
          ))}
        </ul>
      )}

      {/* Add control. Enter on the input is equivalent to clicking
          the +Add button — fastest path for users adding several
          dirs in a row. */}
      <div className="mt-3 flex gap-2">
        <input
          type="text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              handleAdd();
            }
          }}
          placeholder={
            // Different placeholder per OS so the example feels
            // like an actual path the user might paste in. We
            // detect Windows via the same navigator.platform sniff
            // the rest of the settings page uses.
            navigator.platform.toLowerCase().includes("win")
              ? "C:\\Users\\you\\.local\\bin"
              : "/Users/you/.local/bin"
          }
          className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-1 text-sm font-mono"
          aria-label="Directory to add to binary search paths"
          disabled={saving}
        />
        <Button
          variant="outline"
          size="sm"
          onClick={handleAdd}
          disabled={saving || !draft.trim()}
        >
          <Plus className="h-3.5 w-3.5" /> Add
        </Button>
      </div>

      {/* Drift indicator. Hidden on the happy path so the row stays
          unobtrusive; only surfaces when the daemon's view doesn't
          match the persisted list. */}
      {drifted && (
        <div className="mt-2 text-xs text-amber-600 dark:text-amber-400">
          Daemon has a different list applied. It should sync on the
          next change — try adding or removing an entry to refresh.
        </div>
      )}
    </div>
  );
}

function CheckpointsGlobalRow() {
  const { settings, setEnabled } = useCheckpointSettings();
  const [pending, setPending] = React.useState(false);

  async function handleChange(next: boolean) {
    if (pending) return;
    setPending(true);
    try {
      await setEnabled(next);
    } catch {
      // Hook already toasts the underlying error — swallow here so
      // the row stays in a consistent state. The daemon broadcast
      // will reconcile if the write partially succeeded.
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Capture workspace snapshots</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          Let Flowstate snapshot the workspace at each message so you
          can revert file edits from the chat. Turning this off stops
          new captures immediately; existing snapshots stay on disk
          until the sessions they belong to are deleted.
        </div>
      </div>
      <Switch
        checked={settings.globalEnabled}
        onCheckedChange={handleChange}
        disabled={pending}
        aria-label="Capture workspace snapshots"
      />
    </div>
  );
}

function DefaultModelRow() {
  const { state } = useApp();
  const { isProviderEnabled } = useProviderEnabled();
  const [defaults, setDefaults] = React.useState<
    Record<ProviderKind, string | null>
  >({
    claude: null,
    claude_cli: null,
    codex: null,
    github_copilot: null,
    github_copilot_cli: null,
    opencode: null,
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
          isProviderEnabled(entry.kind) &&
          entry.provider.status === "ready" &&
          entry.provider.models.length > 0,
      ),
    [state.providers, isProviderEnabled],
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

function KeyboardShortcutsRow() {
  // Local state — the cheatsheet dialog is self-contained and works
  // wherever it's mounted. No need to plumb open/close into AppShell
  // (it has its own ⌘⇧? bridge); this row just exposes a mouse path.
  // The future per-shortcut editor lands here, replacing the button
  // with a `<ShortcutsList />` table reading the same registry.
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium">All keyboard shortcuts</div>
          <div className="mt-0.5 text-xs text-muted-foreground">
            Browse every shortcut available in Flowstate. Custom rebinding
            is coming soon — for now, the bindings shipped with the app
            are the active set.
          </div>
        </div>
        <Button variant="outline" size="sm" onClick={() => setOpen(true)}>
          Show all shortcuts
        </Button>
      </div>
      <ShortcutsDialog open={open} onOpenChange={setOpen} />
    </>
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
  onUpgrade,
  upgrading,
}: {
  kind: ProviderKind;
  provider: ProviderStatus | undefined;
  onRefresh: () => void;
  refreshing: boolean;
  onToggleEnabled: (enabled: boolean) => void;
  onUpgrade: () => void;
  upgrading: boolean;
}) {
  const { isProviderEnabled } = useProviderEnabled();
  const label = PROVIDER_LABELS[kind];
  const modelCount = provider?.models.length ?? 0;
  const isReady = provider?.status === "ready";
  const enabled = isProviderEnabled(kind);
  const updateAvailable = enabled && (provider?.updateAvailable ?? false);
  const statusText = !enabled
    ? "Disabled"
    : provider
      ? provider.status === "ready"
        ? `${modelCount} model${modelCount === 1 ? "" : "s"}`
        : (provider.message ?? provider.status)
      : "checking...";
  // Trim the leading "v" so "v0.0.41" / "0.0.41" both render as
  // `v0.0.41` consistently. Sentinel strings the daemon may return
  // (e.g. "bundled" for the Claude SDK adapter falling back to its
  // vendored binary) are kept verbatim — `versionLabel` below
  // detects whether to prepend a `v`.
  const installedVersion = provider?.version
    ? provider.version.trim().replace(/^v/i, "")
    : null;
  const isBundled = installedVersion === "bundled";
  const versionLabel = installedVersion
    ? /^[\d]/.test(installedVersion)
      ? `v${installedVersion}`
      : installedVersion
    : null;

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
        <div className="flex items-center gap-2 truncate text-sm font-medium">
          <span className="truncate">{label}</span>
          {updateAvailable ? (
            // Soft amber dot — no toast, no banner. The Upgrade
            // button next to the row is the actionable affordance;
            // this is just the "you have a notification" indicator.
            <span
              className="inline-block h-1.5 w-1.5 shrink-0 rounded-full bg-amber-500"
              aria-label="Update available"
              title={
                provider?.latestVersion
                  ? `Update available (v${provider.latestVersion})`
                  : "Update available"
              }
            />
          ) : null}
        </div>
        <div className="truncate text-xs text-muted-foreground">
          {statusText}
          {versionLabel ? (
            <span
              className="ml-2 font-mono text-[11px] text-muted-foreground/80"
              title={
                isBundled
                  ? "Using the SDK's vendored claude-code binary. Install claude on PATH (or add its directory to Settings → Extra binary search paths) to switch to a local install."
                  : undefined
              }
            >
              {versionLabel}
            </span>
          ) : null}
        </div>
      </div>
      {/* Always render the Upgrade button so the affordance has a
          stable place in the row. Disabled when no update is
          available (or the provider is off / not ready / already
          upgrading), enabled only when the daemon's update probe
          flagged this provider as outdated. The amber dot next to
          the label remains the passive notification — the button
          itself stays present whether or not there's something to
          upgrade. */}
      <Button
        variant="outline"
        size="sm"
        disabled={!updateAvailable || upgrading || !enabled}
        onClick={onUpgrade}
        title={
          !enabled
            ? `${label} is disabled`
            : updateAvailable
              ? provider?.latestVersion
                ? `Upgrade to v${provider.latestVersion}`
                : "Upgrade to the latest version"
              : isBundled
                ? "Bundled with Flowstate — updates ship with the app itself"
                : "Up to date"
        }
      >
        {upgrading ? <Loader2 className="animate-spin" /> : <ArrowUpCircle />}
        Upgrade
      </Button>
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

// "Install CLI" row. Renders the current install status and a
// pair of install buttons. The user picks where the bundled `flow`
// binary should land:
//
//   - "Install for me" → ~/.local/bin/flow on macOS/Linux,
//                        %LOCALAPPDATA%\Programs\flowstate\bin on
//                        Windows. No password prompt; the row
//                        also shows whether the install dir is
//                        on the user's $PATH so they can fix it
//                        manually if not.
//   - "Install system-wide" → /usr/local/bin/flow via osascript /
//                             pkexec on macOS/Linux. Hidden on
//                             Windows (per-user install only in v1).
//
// On mount we call install_cli_status to seed the display so a
// returning user sees "Installed at /Users/foo/.local/bin/flow"
// instead of a fresh-install state. After a successful install
// the status is re-fetched so the UI reflects the new state.
function CliInstallRow() {
  const [status, setStatus] = React.useState<InstallCliStatus | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState<InstallCliTarget | null>(null);
  const isWindows = navigator.userAgent.includes("Windows");

  const refresh = React.useCallback(async () => {
    try {
      const s = await installCliStatus();
      setStatus(s);
      setError(null);
    } catch (err) {
      setError(String(err));
    }
  }, []);

  React.useEffect(() => {
    void refresh();
  }, [refresh]);

  async function handleInstall(target: InstallCliTarget) {
    setBusy(target);
    setError(null);
    try {
      const report = await installCli(target);
      toast({
        description: report.onPath
          ? `Installed flow at ${report.installedPath}. Try it now: \`flow .\` in any new terminal.`
          : `Installed flow at ${report.installedPath}. Add the parent folder to your PATH to use it.`,
        duration: 6000,
      });
      await refresh();
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(null);
    }
  }

  // Heuristic: on macOS and Linux the conventional advice for
  // adding ~/.local/bin to PATH is the same single line in the
  // user's shell rc. We default to zsh (macOS default) but show
  // the bash equivalent as an aside.
  const pathHint = isWindows
    ? "Open a new terminal — newly-launched shells inherit the updated PATH."
    : "Add this line to ~/.zshrc (or ~/.bashrc):\n\n  export PATH=\"$HOME/.local/bin:$PATH\"";

  return (
    <div className="flex flex-col gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="text-sm">
        {status === null && error === null ? (
          <span className="text-muted-foreground">Loading status…</span>
        ) : status?.installed ? (
          <span>
            Installed at{" "}
            <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">
              {status.installedPath}
            </code>
            {!status.pointsAtCurrent && (
              <span className="ml-2 text-amber-600 dark:text-amber-400">
                (points at an older Flowstate — reinstall to refresh)
              </span>
            )}
            {!status.onPath && (
              <span className="ml-2 text-amber-600 dark:text-amber-400">
                (install location not on PATH)
              </span>
            )}
          </span>
        ) : (
          <span className="text-muted-foreground">Not installed.</span>
        )}
      </div>

      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="default"
          onClick={() => handleInstall("user_local")}
          disabled={busy !== null}
        >
          {busy === "user_local" && <Loader2 className="animate-spin" />}
          {status?.installed ? "Reinstall for me" : "Install for me"}
        </Button>
        {!isWindows && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => handleInstall("system")}
            disabled={busy !== null}
          >
            {busy === "system" && <Loader2 className="animate-spin" />}
            Install system-wide (requires password)
          </Button>
        )}
      </div>

      {status && status.installed && !status.onPath && (
        <pre className="overflow-x-auto rounded-md border border-border bg-muted/30 p-2 font-mono text-[11px] text-foreground">
          {pathHint}
        </pre>
      )}

      {error && <div className="text-[11px] text-destructive">{error}</div>}
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

// Manual "Check for updates" row. The app also fires a silent
// startup check from main.tsx; this row lets the user poke it
// on demand and see a result inline via the toast helper. If an
// update is found, the global <UpdateBanner /> (mounted in
// router.tsx) takes over the install UX — we just confirm we
// found one here. Shares the singleton store at `lib/updater.ts`.
function CheckForUpdatesRow() {
  const status = useUpdaterStatus();
  const [version, setVersion] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    getVersion()
      .then((v) => {
        if (!cancelled) setVersion(v);
      })
      .catch(() => {
        if (!cancelled) setVersion(null);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const busy =
    status.kind === "checking" ||
    status.kind === "downloading" ||
    status.kind === "installing";

  async function handleCheck() {
    const result = await checkForUpdate();
    if (result.kind === "up-to-date") {
      toast({
        description: `You're on the latest version of Flowstate${
          version ? ` (v${version})` : ""
        }.`,
        duration: 3000,
      });
      resetUpdaterStatus();
    } else if (result.kind === "error") {
      toast({
        description: `Update check failed: ${result.message}`,
        duration: 4000,
      });
      resetUpdaterStatus();
    } else if (result.kind === "available") {
      toast({
        description: `Flowstate ${result.update.version} is available — see the banner to install.`,
        duration: 4000,
      });
    }
  }

  const buttonLabel =
    status.kind === "checking"
      ? "Checking…"
      : status.kind === "downloading"
        ? "Downloading…"
        : status.kind === "installing"
          ? "Installing…"
          : "Check now";

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">Check for updates</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          {version ? `You're running Flowstate v${version}. ` : ""}
          Flowstate checks for signed updates from GitHub Releases at
          startup; use this to poll on demand.
        </div>
      </div>
      <Button
        variant="outline"
        size="sm"
        disabled={busy}
        onClick={() => void handleCheck()}
      >
        {busy ? <Loader2 className="animate-spin" /> : <RefreshCw />}
        {buttonLabel}
      </Button>
    </div>
  );
}

// User-facing labels for the wire-format provisioning phase ids.
// Centralised so the banner copy and toast messages stay consistent.
const PROVISION_PHASE_LABELS: Record<string, string> = {
  node: "Node.js runtime",
  "claude-sdk": "Claude SDK",
  "copilot-sdk": "GitHub Copilot SDK",
};

function provisionPhaseLabel(phase: string): string {
  return PROVISION_PHASE_LABELS[phase] ?? phase;
}

// One row inside <ProvisionErrorsBanner /> — renders the failure
// summary, a Retry button, and a disclosure that reveals the full
// multi-line anyhow error string. Local state for retry-in-flight so
// each row spins independently when the user retries multiple
// failures back-to-back.
function ProvisionFailureRow({
  phase,
  error,
}: {
  phase: string;
  error: string;
}) {
  const [retrying, setRetrying] = React.useState(false);
  const [showFull, setShowFull] = React.useState(false);
  // Show only the first line in the collapsed state. The Rust side
  // formats anyhow's `{:?}` which is multi-line with cause chains;
  // the first line is the most actionable summary.
  const firstLine = React.useMemo(
    () => error.split("\n")[0]?.trim() || "Unknown error",
    [error],
  );

  async function handleRetry() {
    setRetrying(true);
    try {
      await retryProvisionPhase(phase);
      // Success — the `provision` event listener in app-store will
      // dispatch `clear_provision_failure`, which removes this row.
      toast({
        description: `${provisionPhaseLabel(phase)} provisioned successfully.`,
        duration: 3000,
      });
    } catch (err) {
      toast({
        description: `Retry failed: ${(err as Error).message ?? String(err)}`,
        duration: 5000,
      });
    } finally {
      setRetrying(false);
    }
  }

  return (
    <div className="border-b border-destructive/20 px-4 py-3 last:border-b-0">
      <div className="flex items-start gap-3">
        <AlertCircle className="mt-0.5 h-4 w-4 shrink-0 text-destructive" />
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium">
            {provisionPhaseLabel(phase)} install failed
          </div>
          <div className="mt-0.5 break-words text-xs text-muted-foreground">
            {firstLine}
          </div>
          {showFull && (
            <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap rounded-md bg-muted/40 p-2 font-mono text-[11px] text-foreground">
              {error}
            </pre>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setShowFull((v) => !v)}
            aria-label={showFull ? "Hide full error" : "Show full error"}
          >
            {showFull ? (
              <ChevronUp className="h-3.5 w-3.5" />
            ) : (
              <ChevronDown className="h-3.5 w-3.5" />
            )}
          </Button>
          <Button
            variant="default"
            size="sm"
            onClick={handleRetry}
            disabled={retrying}
          >
            {retrying ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <RefreshCw className="h-3.5 w-3.5" />
            )}
            Retry
          </Button>
        </div>
      </div>
    </div>
  );
}

// Top-of-page banner listing every runtime-provisioning failure
// from the most recent boot (or from a user-initiated retry that
// itself failed). Renders nothing when everything provisioned
// cleanly, so the warm-launch case has zero visual cost.
function ProvisionErrorsBanner() {
  const failures = useProvisionFailures();
  if (failures.length === 0) return null;
  return (
    <section className="mb-8">
      <div className="mb-3">
        <h2 className="text-sm font-semibold text-destructive">
          Setup issues
        </h2>
        <p className="mt-0.5 text-xs text-muted-foreground">
          One or more runtime components failed to install. The rest of
          Flowstate still works, but the affected providers won&apos;t be
          usable until you retry successfully.
        </p>
      </div>
      <div className="overflow-hidden rounded-lg border border-destructive/40 bg-destructive/5">
        {failures.map((f) => (
          <ProvisionFailureRow key={f.phase} phase={f.phase} error={f.error} />
        ))}
      </div>
    </section>
  );
}

// Single read-only path row with a Reveal-in-Finder button. Used by
// the Diagnostics section for app-data, logs, and cache directories.
// The reveal button degrades gracefully: if the directory doesn't
// exist yet (e.g. logs dir on first launch before any line was
// written), `revealItemInDir` errors and we surface a toast.
function PathRow({
  label,
  description,
  resolve,
  extraActions,
}: {
  label: string;
  description: string;
  resolve: () => Promise<string>;
  /** Optional render slot for action buttons rendered to the right
   *  of the path input, after the Reveal button. Receives the
   *  resolved path so actions can target it (or `null` while still
   *  loading / on resolve error). */
  extraActions?: (path: string | null) => React.ReactNode;
}) {
  const [path, setPath] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    resolve()
      .then((p) => {
        if (!cancelled) setPath(p);
      })
      .catch((err) => {
        if (!cancelled) setError(String(err));
      });
    return () => {
      cancelled = true;
    };
    // resolve is intentionally unmemoized at call sites — these rows
    // mount once and never refetch. Including it in deps would
    // re-run the effect on every parent render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function handleReveal() {
    if (!path) return;
    try {
      await revealItemInDir(path);
    } catch (err) {
      toast({
        description: `Couldn't open ${label}: ${(err as Error).message ?? String(err)}`,
        duration: 4000,
      });
    }
  }

  return (
    <div className="flex items-start gap-3 border-b border-border px-4 py-3 last:border-b-0">
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">{label}</div>
        <div className="mt-0.5 text-xs text-muted-foreground">
          {description}
        </div>
        <div className="mt-2 flex items-center gap-2">
          {error ? (
            <div className="text-[11px] text-destructive">{error}</div>
          ) : (
            <input
              type="text"
              readOnly
              value={path ?? "Loading…"}
              onFocus={(e) => e.currentTarget.select()}
              onClick={(e) => e.currentTarget.select()}
              className="w-full min-w-0 rounded-md border border-input bg-muted/30 px-2 py-1 font-mono text-[11px] text-foreground"
              aria-label={`${label} path`}
            />
          )}
          <Button
            variant="outline"
            size="sm"
            onClick={handleReveal}
            disabled={!path || !!error}
            aria-label={`Reveal ${label} in file manager`}
          >
            <FolderOpen className="h-3.5 w-3.5" />
            Reveal
          </Button>
          {extraActions?.(path)}
        </div>
      </div>
    </div>
  );
}

// Format `bytes` as a human-readable size — matches macOS Finder's
// "About Mac" style (binary divisor, two-decimal precision once we
// pass MB so a 354 MB cache shows as "354.21 MB" not "0.35 GB").
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(0)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(2)} MB`;
  return `${(mb / 1024).toFixed(2)} GB`;
}

// "Clear cache" button rendered on the Runtime cache row. Confirms
// (the cache is non-trivial to rebuild — ~30s Node download +
// ~60s npm install per provider on a typical connection), invokes
// the Rust `clear_runtime_cache` command, and offers a one-click
// relaunch. Relaunch is necessary because the in-process OnceLocks
// in `embedded-node` and the bridge runtimes still hold paths into
// the now-deleted directory; without a relaunch the next session
// spawn would fail mid-flight rather than re-downloading cleanly.
function ClearCacheButton({ path }: { path: string | null }) {
  const [busy, setBusy] = React.useState(false);

  async function handleClear() {
    if (!path) return;
    const ok = await confirm(
      `Delete the runtime cache at ${path}?\n\nFlowstate will need to redownload Node.js (~30 MB) and reinstall the provider SDKs (~300 MB) on next launch.`,
      { title: "Clear runtime cache", kind: "warning", okLabel: "Delete" },
    );
    if (!ok) return;
    setBusy(true);
    try {
      const freed = await clearRuntimeCache();
      const restart = await confirm(
        `Cleared ${formatBytes(freed)} from the runtime cache.\n\nRelaunch Flowstate now to redownload? (Provider SDK sessions won't work until you do.)`,
        { title: "Cache cleared", kind: "info", okLabel: "Relaunch" },
      );
      if (restart) {
        await relaunch();
      } else {
        toast({
          description: `Cleared ${formatBytes(freed)}. Relaunch when ready.`,
          duration: 4000,
        });
      }
    } catch (err) {
      toast({
        description: `Couldn't clear cache: ${(err as Error).message ?? String(err)}`,
        duration: 5000,
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Button
      variant="outline"
      size="sm"
      onClick={handleClear}
      disabled={!path || busy}
      aria-label="Clear runtime cache"
    >
      {busy ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
      ) : (
        <Trash2 className="h-3.5 w-3.5" />
      )}
      Clear
    </Button>
  );
}

export function SettingsView() {
  const { state, send } = useApp();
  const { setProviderEnabled } = useProviderEnabled();
  const { state: sidebarState } = useSidebar();
  const showMacTrafficSpacer = isMacOS() && sidebarState === "collapsed";
  const [refreshingKind, setRefreshingKind] = React.useState<ProviderKind | null>(
    null,
  );
  const [upgradingKind, setUpgradingKind] = React.useState<ProviderKind | null>(
    null,
  );
  // macOS-only: probe whether the caffeinate Tauri commands are
  // registered. Hides the entire "macOS" group on other platforms
  // without needing a separate platform-detection dependency.
  const caffeinateSupported = useCaffeinateSupport();

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

  async function handleUpgrade(kind: ProviderKind) {
    setUpgradingKind(kind);
    try {
      // Wait for the daemon's Ack/Error response. Runtime-core
      // forces a fresh health probe after the upgrade either way,
      // and the broadcast `ProviderHealthUpdated` event clears the
      // amber dot once the probe lands.
      const reply = await send({ type: "upgrade_provider_cli", provider: kind });
      const description =
        reply && reply.type === "ack"
          ? reply.message
          : `${PROVIDER_LABELS[kind]} upgrade requested.`;
      toast({ description, duration: 3000 });
    } catch (err) {
      toast({
        description: `Failed to upgrade ${PROVIDER_LABELS[kind]}: ${
          (err as Error).message
        }`,
        duration: 4000,
      });
    } finally {
      setUpgradingKind(null);
    }
  }

  function handleToggleEnabled(kind: ProviderKind, enabled: boolean) {
    setProviderEnabled(kind, enabled);
    // Propagate both directions to the daemon: enabling so it starts
    // health-checking / reporting models, disabling so the SDK's
    // `provider_enablement` table reflects the user's choice. The
    // latter is what makes the MCP `list_providers` tool omit
    // disabled providers and what blocks `spawn` calls against them.
    send({ type: "set_provider_enabled", provider: kind, enabled }).catch(
      () => {},
    );
    toast({
      description: `${PROVIDER_LABELS[kind]} ${enabled ? "enabled" : "disabled"}`,
      duration: 2000,
    });
  }

  return (
    <div className="flex h-svh flex-col">
      <header
        data-tauri-drag-region
        className="flex h-9 items-center gap-1 border-b border-border px-2 text-sm"
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        <SidebarTrigger />
        <span className="font-medium">Settings</span>
      </header>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl px-6 py-8">
          {/* Provisioning failures (Node download, SDK npm install)
              live at the very top so they're impossible to miss when
              the sidebar's red dot draws the user here. Renders
              nothing in the happy path — zero visual cost. */}
          <ProvisionErrorsBanner />
          <SettingsGroup
            title="Appearance"
            description="Customize how Flowstate looks."
          >
            <ThemeRow />
            <ContextDisplayRow />
            <VimModeRow />
          </SettingsGroup>
          <SettingsGroup
            title="Keyboard shortcuts"
            description="See every action you can trigger from the keyboard. Press ⌘⇧? from anywhere to pull up the same list as a quick overlay."
          >
            <KeyboardShortcutsRow />
          </SettingsGroup>
          <SettingsGroup
            title="Defaults"
            description="Default values for new sessions. These apply across all providers."
          >
            <DefaultProviderRow />
            <DefaultEffortRow />
            <DefaultPermissionModeRow />
            <StrictPlanModeRow />
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
                onUpgrade={() => void handleUpgrade(kind)}
                upgrading={upgradingKind === kind}
              />
            ))}
          </SettingsGroup>
          <SettingsGroup
            title="MCP servers"
            description="Define MCP servers once and Flowstate registers them with every provider that supports MCP (Claude, Codex, Copilot, OpenCode). Backed by ~/.flowstate/mcp.json. Changes apply to new sessions only."
          >
            <McpServersPanel />
          </SettingsGroup>
          <SettingsGroup
            title="File checkpoints"
            description="Capture a snapshot of each session's workspace at every message so you can revert file edits later. Disk cost is typically a few megabytes per 100 turns on a small project. Turn this off if you'd rather manage rollback with your own git workflow."
          >
            <CheckpointsGlobalRow />
          </SettingsGroup>
          <SettingsGroup
            title="Performance"
            description="Tune how Flowstate uses your machine's resources."
          >
            <PoolSizeRow />
          </SettingsGroup>
          {caffeinateSupported && (
            <SettingsGroup
              title="macOS"
              description="Settings that only apply on macOS."
            >
              <CaffeinateRow />
            </SettingsGroup>
          )}
          <SettingsGroup
            title="Git worktrees"
            description="Controls for where new git worktrees land on disk."
          >
            <WorktreeBasePathRow />
          </SettingsGroup>
          <SettingsGroup
            title="Command line"
            description="Run `flow .` (or `flow <dir>`) from any terminal to open a new thread on a project, using your saved default provider, model, and permission mode."
          >
            <CliInstallRow />
          </SettingsGroup>
          <SettingsGroup
            title="Provider CLI discovery"
            description="Where Flowstate looks for provider CLIs (claude, codex, copilot, opencode). The resolver always checks PATH and a curated list of common install locations first — these extra directories are an escape hatch for installs the auto-detection misses, especially on Windows where GUI-launched processes inherit a narrower PATH than your shell."
          >
            <BinarySearchPathsRow />
          </SettingsGroup>
          <SettingsGroup
            title="Diagnostics"
            description="On-disk locations Flowstate uses. Click Reveal to open the folder in Finder / Explorer / your file manager — useful when troubleshooting or sharing logs."
          >
            <AppDataDirRow />
            <PathRow
              label="Logs"
              description="`flowstate.log` is appended here. Send this file when reporting bugs."
              resolve={getLogDir}
            />
            <PathRow
              label="Runtime cache"
              description="Embedded Node.js + provider SDK node_modules (~350 MB after first launch). Clear this if a botched first install left the cache in a bad state — Flowstate will redownload on next launch."
              resolve={getCacheDir}
              extraActions={(path) => <ClearCacheButton path={path} />}
            />
          </SettingsGroup>
          <SettingsGroup
            title="Updates"
            description="Keep Flowstate up to date. Updates are cryptographically signed and delivered via GitHub Releases."
          >
            <CheckForUpdatesRow />
          </SettingsGroup>
        </div>
      </div>
    </div>
  );
}
