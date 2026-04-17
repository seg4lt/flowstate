import { Loader2 } from "lucide-react";
import type { ProviderKind, ProviderStatus } from "@/lib/types";

export const PROVIDER_COLORS: Record<ProviderKind, string> = {
  claude: "bg-amber-500",
  claude_cli: "bg-purple-500",
  codex: "bg-green-500",
  github_copilot: "bg-blue-500",
  github_copilot_cli: "bg-cyan-500",
};

// Shown before health checks complete
export const ALL_PROVIDERS: { kind: ProviderKind; label: string }[] = [
  { kind: "claude", label: "Claude" },
  { kind: "claude_cli", label: "Claude 2" },
  { kind: "codex", label: "Codex" },
  { kind: "github_copilot", label: "GitHub Copilot" },
  { kind: "github_copilot_cli", label: "GitHub Copilot 2" },
];

export function statusBadge(provider: ProviderStatus | undefined) {
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
