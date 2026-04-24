import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Loader2, SquarePen } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";
import { readDefaultModel } from "@/lib/defaults-settings";
import { rememberPickedModel } from "@/lib/model-settings";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { ALL_PROVIDERS, PROVIDER_COLORS, statusBadge } from "./provider-constants";
import { SearchableModelSubMenu } from "./searchable-model-submenu";

interface ProviderDropdownProps {
  projectId?: string;
  /** Filesystem path of the project the new thread will target. When
   *  provided, picking a provider fires a fire-and-forget invalidation
   *  of the cached git branch for this path so the new ChatView (or
   *  the upstream consumer via `onSelect`) sees a fresh branch even
   *  if git state moved out-of-band since the last fetch. */
  projectPath?: string;
  /** Override the default pencil-icon trigger. Used by the project-home
   *  page to render the action as a sized button instead of the tiny
   *  hover-only icon the sidebar shows. */
  trigger?: React.ReactElement;
  /** When provided, the dropdown calls this instead of creating a new
   *  thread itself. Useful when the caller needs to run extra setup
   *  (e.g. worktree provisioning) before starting the session. */
  onSelect?: (provider: ProviderKind, model?: string) => void;
}

export function ProviderDropdown({
  projectId,
  projectPath,
  trigger,
  onSelect,
}: ProviderDropdownProps) {
  const { state, send } = useApp();
  const { isProviderEnabled } = useProviderEnabled();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

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
    // Fire-and-forget branch refresh so the ChatView (or the onSelect
    // consumer, which typically navigates into a worktree thread)
    // picks up out-of-band `git checkout` changes without blocking.
    if (projectPath) {
      void queryClient.invalidateQueries({
        queryKey: ["git", "branch", projectPath],
      });
    }
    // Resolution priority for the session's starting model:
    //   1. explicit pick from the submenu,
    //   2. user's saved default for this provider (`readDefaultModel`),
    //   3. the provider catalog's first entry — which, for the Claude
    //      SDK bridge, is exactly what `q.supportedModels()` returns
    //      first and therefore the SDK's own default.
    //
    // Step 3 is what fixes the "model chip shows 'Default' until the
    // first message" bug: passing an explicit value here means
    // `session.summary.model` is populated at spawn time, so the
    // toolbar renders the real model label on first paint instead of
    // waiting for the `model_resolved` event on turn 1.
    const resolvedModel =
      model ??
      defaultModels.get(provider) ??
      providerMap.get(provider)?.models[0]?.value;
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
      // Remember the alias we spawned with so the composer toolbar's
      // capability lookups (effort / adaptive) survive the SDK
      // replacing `session.model` with a pinned id on turn 1 — see
      // the rationale in `lib/model-settings.ts`.
      if (resolvedModel) {
        rememberPickedModel(res.session.sessionId, resolvedModel);
      }
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
                <SearchableModelSubMenu
                  models={info.models}
                  onSelect={(modelValue) => createThread(kind, modelValue)}
                />
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
