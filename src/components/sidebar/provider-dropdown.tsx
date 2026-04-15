import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { Loader2, SquarePen } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { ProviderKind, ProviderStatus } from "@/lib/types";
import { readDefaultModel } from "@/lib/defaults-settings";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";

const PROVIDER_COLORS: Record<ProviderKind, string> = {
  claude: "bg-amber-500",
  claude_cli: "bg-purple-500",
  codex: "bg-green-500",
  github_copilot: "bg-blue-500",
  github_copilot_cli: "bg-cyan-500",
};

// Shown before health checks complete
const ALL_PROVIDERS: { kind: ProviderKind; label: string }[] = [
  { kind: "claude", label: "Claude" },
  { kind: "claude_cli", label: "Claude 2" },
  { kind: "codex", label: "Codex" },
  { kind: "github_copilot", label: "GitHub Copilot" },
  { kind: "github_copilot_cli", label: "GitHub Copilot 2" },
];

function statusBadge(provider: ProviderStatus | undefined) {
  if (!provider) {
    return (
      <span className="ml-auto flex items-center gap-1 text-[10px] text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
      </span>
    );
  }
  if (provider.status === "ready") return null;
  if (provider.status === "warning") {
    return (
      <span className="ml-auto text-[10px] text-yellow-500">
        {provider.message ?? "warning"}
      </span>
    );
  }
  return (
    <span className="ml-auto text-[10px] text-muted-foreground">
      {provider.message ?? "unavailable"}
    </span>
  );
}

interface ProviderDropdownProps {
  projectId?: string;
  /** Override the default pencil-icon trigger. Used by the project-home
   *  page to render the action as a sized button instead of the tiny
   *  hover-only icon the sidebar shows. */
  trigger?: React.ReactElement;
  /** When provided, the dropdown calls this instead of creating a new
   *  thread itself. Useful when the caller needs to run extra setup
   *  (e.g. worktree provisioning) before starting the session. */
  onSelect?: (provider: ProviderKind, model?: string) => void;
}

export function ProviderDropdown({ projectId, trigger, onSelect }: ProviderDropdownProps) {
  const { state, send } = useApp();
  const { isProviderEnabled } = useProviderEnabled();
  const navigate = useNavigate();

  const providerMap = new Map(state.providers.map((p) => [p.kind, p]));
  const stillLoading = !state.ready;

  // Pre-load per-provider default models from user settings so that
  // clicking a provider (without picking a specific model from the
  // submenu) uses the user's configured default.
  const [defaultModels, setDefaultModels] = React.useState<
    Map<ProviderKind, string>
  >(new Map());

  React.useEffect(() => {
    let cancelled = false;
    const readyProviders = state.providers.filter(
      (p) => isProviderEnabled(p.kind) && p.status === "ready",
    );
    Promise.all(
      readyProviders.map(async (p) => {
        const model = await readDefaultModel(p.kind);
        return [p.kind, model] as const;
      }),
    ).then((entries) => {
      if (cancelled) return;
      const map = new Map<ProviderKind, string>();
      for (const [kind, model] of entries) {
        if (model) map.set(kind, model);
      }
      setDefaultModels(map);
    });
    return () => {
      cancelled = true;
    };
  }, [state.providers]);

  async function createThread(provider: ProviderKind, model?: string) {
    const resolvedModel = model ?? defaultModels.get(provider);
    if (onSelect) {
      onSelect(provider, resolvedModel);
      return;
    }
    const res = await send({
      type: "start_session",
      provider,
      model: resolvedModel,
      project_id: projectId,
    });
    if (res && res.type === "session_created") {
      navigate({
        to: "/chat/$sessionId",
        params: { sessionId: res.session.sessionId },
      });
    }
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        {trigger ?? (
          <button
            type="button"
            className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/project:opacity-100"
            onClick={(e) => e.stopPropagation()}
          >
            <SquarePen className="h-3.5 w-3.5" />
          </button>
        )}
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        {stillLoading && (
          <>
            <DropdownMenuLabel className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              Checking providers...
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
          </>
        )}

        {ALL_PROVIDERS.map(({ kind, label }) => {
          const info = providerMap.get(kind);
          // Disabled providers are hidden from the new-session picker
          // entirely. Users re-enable them from Settings, which updates
          // the app-level context and they reappear here without a reload.
          if (!isProviderEnabled(kind)) return null;
          const isReady = info?.status === "ready";
          const hasModels = info && info.models.length > 0;

          if (hasModels && isReady) {
            return (
              <DropdownMenuSub key={kind}>
                <DropdownMenuSubTrigger>
                  <span
                    className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[kind]}`}
                  />
                  New {label} thread
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  {info.models.map((model) => (
                    <DropdownMenuItem
                      key={model.value}
                      onClick={() => createThread(kind, model.value)}
                    >
                      {model.label}
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            );
          }

          return (
            <DropdownMenuItem
              key={kind}
              disabled={!isReady}
              onClick={() => isReady && createThread(kind)}
            >
              <span
                className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${isReady ? PROVIDER_COLORS[kind] : "bg-muted-foreground/30"}`}
              />
              New {label} thread
              {statusBadge(info)}
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
