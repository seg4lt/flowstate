import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";

export function NewThreadPage() {
  const { state, send } = useApp();
  const navigate = useNavigate();
  const [creating, setCreating] = React.useState(false);

  const availableProviders = state.providers.filter((p) => p.installed);
  const [selectedProvider, setSelectedProvider] =
    React.useState<ProviderKind | null>(null);
  const [selectedModel, setSelectedModel] = React.useState<string>("");

  const provider = selectedProvider
    ? state.providers.find((p) => p.kind === selectedProvider)
    : null;

  // Auto-select first provider when available
  React.useEffect(() => {
    if (!selectedProvider && availableProviders.length > 0) {
      setSelectedProvider(availableProviders[0].kind);
    }
  }, [availableProviders, selectedProvider]);

  async function handleCreate() {
    if (!selectedProvider || creating) return;
    setCreating(true);
    const res = await send({
      type: "start_session",
      provider: selectedProvider,
      model: selectedModel || undefined,
    });
    if (res && res.type === "session_created") {
      navigate({
        to: "/chat/$sessionId",
        params: { sessionId: res.session.sessionId },
      });
    }
    setCreating(false);
  }

  if (!state.ready) {
    return (
      <div className="flex h-full min-h-svh flex-col">
        <header className="flex h-12 items-center gap-2 border-b border-border px-2 text-sm text-muted-foreground">
          <SidebarTrigger />
          <span>Loading...</span>
        </header>
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          Connecting to daemon...
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-svh flex-col">
      <header className="flex h-12 items-center gap-2 border-b border-border px-2 text-sm text-muted-foreground">
        <SidebarTrigger />
        <span>New thread</span>
      </header>

      <div className="flex flex-1 items-center justify-center p-8">
        <div className="w-full max-w-sm space-y-4">
          <h2 className="text-center text-lg font-semibold">Start a new thread</h2>

          {/* Provider selection */}
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">
              Provider
            </label>
            <div className="grid gap-2">
              {availableProviders.map((p) => (
                <button
                  key={p.kind}
                  type="button"
                  onClick={() => {
                    setSelectedProvider(p.kind);
                    setSelectedModel("");
                  }}
                  className={`flex items-center justify-between rounded-lg border px-3 py-2 text-left text-sm transition-colors ${
                    selectedProvider === p.kind
                      ? "border-primary bg-primary/5"
                      : "border-border hover:bg-muted/50"
                  }`}
                >
                  <div>
                    <span className="font-medium">{p.label}</span>
                    {p.version && (
                      <span className="ml-2 text-xs text-muted-foreground">
                        v{p.version}
                      </span>
                    )}
                  </div>
                  <span
                    className={`text-xs ${
                      p.status === "ready"
                        ? "text-green-600 dark:text-green-400"
                        : p.status === "warning"
                          ? "text-yellow-600 dark:text-yellow-400"
                          : "text-destructive"
                    }`}
                  >
                    {p.status}
                  </span>
                </button>
              ))}
              {availableProviders.length === 0 && (
                <p className="text-center text-sm text-muted-foreground">
                  No providers available. Check your installation.
                </p>
              )}
            </div>
          </div>

          {/* Model selection */}
          {provider && provider.models.length > 0 && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">
                Model
              </label>
              <select
                value={selectedModel}
                onChange={(e) => setSelectedModel(e.target.value)}
                className="w-full rounded-lg border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <option value="">Default</option>
                {provider.models.map((m) => (
                  <option key={m.value} value={m.value}>
                    {m.label}
                  </option>
                ))}
              </select>
            </div>
          )}

          {/* Create button */}
          <button
            type="button"
            onClick={handleCreate}
            disabled={!selectedProvider || creating}
            className="flex w-full items-center justify-center gap-2 rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:pointer-events-none disabled:opacity-50"
          >
            <Plus className="h-4 w-4" />
            {creating ? "Creating..." : "New thread"}
          </button>
        </div>
      </div>
    </div>
  );
}
