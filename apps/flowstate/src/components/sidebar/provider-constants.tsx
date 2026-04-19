import { Loader2 } from "lucide-react";
import type { ProviderStatus } from "@/lib/types";

// Back-compat re-exports — the canonical source is `@/lib/providers`.
// New code should import from there directly.
export { ALL_PROVIDERS, PROVIDER_COLORS } from "@/lib/providers";

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
