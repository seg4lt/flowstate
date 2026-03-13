import * as React from "react";
import { Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Switch } from "@/components/ui/switch";
import { useApp } from "@/stores/app-store";
import { toast } from "@/hooks/use-toast";
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
        </div>
      </div>
    </div>
  );
}
