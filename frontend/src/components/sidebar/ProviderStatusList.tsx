import { SidebarMenuButton, SidebarMenuItem } from "../ui/sidebar";
import type { ProviderStatus, ProviderStatusLevel } from "../../types";

const STATUS_COLORS: Record<ProviderStatusLevel, string> = {
  ready: "bg-green-500",
  warning: "bg-yellow-500",
  error: "bg-red-500",
};

export function ProviderStatusList({ providers }: { providers: ProviderStatus[] }) {
  return (
    <>
      {providers.map((provider) => {
        const message = provider.installed
          ? provider.authenticated
            ? "Ready"
            : "Not authenticated"
          : "Not installed";
        return (
          <SidebarMenuItem key={provider.kind}>
            <SidebarMenuButton disabled className="gap-2">
              <div className={`w-2 h-2 rounded-full shrink-0 ${STATUS_COLORS[provider.status]}`} />
              <span className="flex-1 min-w-0 truncate">{provider.label}</span>
              <span className="text-[10px] text-muted-foreground">{message}</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        );
      })}
    </>
  );
}
