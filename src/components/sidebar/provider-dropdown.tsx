import { useNavigate } from "@tanstack/react-router";
import { SquarePen } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";

const PROVIDER_COLORS: Record<ProviderKind, string> = {
  claude: "bg-amber-500",
  claude_cli: "bg-purple-500",
  codex: "bg-green-500",
  github_copilot: "bg-blue-500",
  github_copilot_cli: "bg-cyan-500",
};

interface ProviderDropdownProps {
  projectId?: string;
}

export function ProviderDropdown({ projectId }: ProviderDropdownProps) {
  const { state, send } = useApp();
  const navigate = useNavigate();

  const availableProviders = state.providers.filter((p) => p.installed);

  async function createThread(provider: ProviderKind, model?: string) {
    const res = await send({
      type: "start_session",
      provider,
      model,
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
        <button
          type="button"
          className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/project:opacity-100"
          onClick={(e) => e.stopPropagation()}
        >
          <SquarePen className="h-3.5 w-3.5" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        {availableProviders.map((provider) =>
          provider.models.length > 0 ? (
            <DropdownMenuSub key={provider.kind}>
              <DropdownMenuSubTrigger>
                <span
                  className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[provider.kind]}`}
                />
                New {provider.label} thread
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                <DropdownMenuItem
                  onClick={() => createThread(provider.kind)}
                >
                  Default model
                </DropdownMenuItem>
                {provider.models.map((model) => (
                  <DropdownMenuItem
                    key={model.value}
                    onClick={() => createThread(provider.kind, model.value)}
                  >
                    {model.label}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          ) : (
            <DropdownMenuItem
              key={provider.kind}
              onClick={() => createThread(provider.kind)}
            >
              <span
                className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[provider.kind]}`}
              />
              New {provider.label} thread
            </DropdownMenuItem>
          ),
        )}
        {availableProviders.length === 0 && (
          <DropdownMenuItem disabled>No providers available</DropdownMenuItem>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
