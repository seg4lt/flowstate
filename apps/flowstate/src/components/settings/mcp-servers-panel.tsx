// Settings panel for the user's global MCP server list. Reads /
// writes `~/.flowstate/mcp.json` via the `get_user_mcp_servers` /
// `set_user_mcp_servers` Tauri commands. The same file is loaded
// on every session spawn by the Rust adapters, so adds/edits
// applied here propagate to the next session of every provider —
// running sessions keep their existing config (documented limit).

import * as React from "react";
import { Plus, Pencil, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { toast } from "@/hooks/use-toast";
import {
  RESERVED_MCP_NAME,
  getMcpServers,
  setMcpServers,
  validateMcpEntry,
  type McpServerConfig,
} from "@/lib/mcp-servers";

type DraftEntry = {
  /** Empty when adding; populated when editing an existing row. */
  originalName: string | null;
  name: string;
  type: "stdio" | "http" | "sse";
  command: string;
  args: string;
  envText: string;
  url: string;
};

const EMPTY_DRAFT: DraftEntry = {
  originalName: null,
  name: "",
  type: "stdio",
  command: "",
  args: "",
  envText: "",
  url: "",
};

/** Encode an `McpServerConfig` into the form's flat string fields. */
function configToDraft(name: string, cfg: McpServerConfig): DraftEntry {
  return {
    originalName: name,
    name,
    type: cfg.type,
    command: cfg.command ?? "",
    args: (cfg.args ?? []).join(" "),
    envText: cfg.env
      ? Object.entries(cfg.env)
          .map(([k, v]) => `${k}=${v}`)
          .join("\n")
      : "",
    url: cfg.url ?? "",
  };
}

/** Parse the form draft back into an `McpServerConfig`. Throws on
 *  malformed `envText` so the dialog can flag the row inline. */
function draftToConfig(draft: DraftEntry): McpServerConfig {
  if (draft.type === "stdio") {
    const args = draft.args
      .split(/\s+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    const env: Record<string, string> = {};
    for (const raw of draft.envText.split("\n")) {
      const line = raw.trim();
      if (!line) continue;
      const eq = line.indexOf("=");
      if (eq < 0) {
        throw new Error(`env line missing '=': ${line}`);
      }
      const key = line.slice(0, eq).trim();
      const value = line.slice(eq + 1);
      if (!key) {
        throw new Error(`env line missing key: ${line}`);
      }
      env[key] = value;
    }
    return {
      type: "stdio",
      command: draft.command.trim(),
      args,
      env: Object.keys(env).length > 0 ? env : undefined,
    };
  }
  return {
    type: draft.type,
    url: draft.url.trim(),
  };
}

export function McpServersPanel() {
  const [servers, setServers] = React.useState<Record<
    string,
    McpServerConfig
  > | null>(null);
  const [dialogOpen, setDialogOpen] = React.useState(false);
  const [draft, setDraft] = React.useState<DraftEntry>(EMPTY_DRAFT);
  const [draftError, setDraftError] = React.useState<string | null>(null);
  const [saving, setSaving] = React.useState(false);

  const reload = React.useCallback(async () => {
    try {
      const file = await getMcpServers();
      setServers(file.mcpServers ?? {});
    } catch (err) {
      console.error("[mcp-servers] failed to load", err);
      setServers({});
    }
  }, []);

  React.useEffect(() => {
    void reload();
  }, [reload]);

  const openAdd = () => {
    setDraft(EMPTY_DRAFT);
    setDraftError(null);
    setDialogOpen(true);
  };

  const openEdit = (name: string) => {
    if (!servers) return;
    const cfg = servers[name];
    if (!cfg) return;
    setDraft(configToDraft(name, cfg));
    setDraftError(null);
    setDialogOpen(true);
  };

  const persist = async (next: Record<string, McpServerConfig>) => {
    setSaving(true);
    try {
      const written = await setMcpServers(next);
      setServers(written.mcpServers ?? {});
      return true;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      toast({
        description: `Failed to save MCP servers: ${msg}`,
        duration: 4000,
      });
      return false;
    } finally {
      setSaving(false);
    }
  };

  const handleSave = async () => {
    if (!servers) return;
    let cfg: McpServerConfig;
    try {
      cfg = draftToConfig(draft);
    } catch (err) {
      setDraftError(err instanceof Error ? err.message : String(err));
      return;
    }
    const validationError = validateMcpEntry(draft.name, cfg);
    if (validationError) {
      setDraftError(validationError);
      return;
    }
    const next = { ...servers };
    // If renaming an existing entry, drop the old key.
    if (draft.originalName && draft.originalName !== draft.name) {
      delete next[draft.originalName];
    }
    next[draft.name] = cfg;
    const ok = await persist(next);
    if (ok) {
      setDialogOpen(false);
      toast({
        description: `Saved MCP server "${draft.name}". Active in new sessions.`,
        duration: 3000,
      });
    }
  };

  const handleDelete = async (name: string) => {
    if (!servers) return;
    const next = { ...servers };
    delete next[name];
    const ok = await persist(next);
    if (ok) {
      toast({
        description: `Removed MCP server "${name}".`,
        duration: 2500,
      });
    }
  };

  const entries = servers ? Object.entries(servers) : [];

  return (
    <div className="px-4 py-3">
      {servers === null ? (
        <div className="text-xs text-muted-foreground">Loading…</div>
      ) : entries.length === 0 ? (
        <div className="text-xs text-muted-foreground">
          No MCP servers configured. Add one below — it will be available to
          every provider on the next session.
        </div>
      ) : (
        <div className="divide-y divide-border">
          {entries.map(([name, cfg]) => (
            <McpServerRow
              key={name}
              name={name}
              cfg={cfg}
              onEdit={() => openEdit(name)}
              onDelete={() => void handleDelete(name)}
              disabled={saving}
            />
          ))}
        </div>
      )}
      <div className="mt-3 flex justify-start">
        <Button
          size="sm"
          variant="outline"
          onClick={openAdd}
          disabled={saving || servers === null}
        >
          <Plus className="mr-1 h-3.5 w-3.5" />
          Add MCP server
        </Button>
      </div>

      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {draft.originalName ? "Edit MCP server" : "Add MCP server"}
            </DialogTitle>
            <DialogDescription>
              Servers added here register with every provider that supports
              MCP. Changes apply to the next session you start.
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-3">
            <div>
              <label
                htmlFor="mcp-name"
                className="mb-1 block text-xs font-medium"
              >
                Name
              </label>
              <Input
                id="mcp-name"
                value={draft.name}
                onChange={(e) =>
                  setDraft({ ...draft, name: e.target.value })
                }
                placeholder="e.g. sqlite"
                aria-label="MCP server name"
              />
              {draft.name === RESERVED_MCP_NAME && (
                <p className="mt-1 text-[11px] text-destructive">
                  &quot;{RESERVED_MCP_NAME}&quot; is reserved.
                </p>
              )}
            </div>

            <div>
              <label
                htmlFor="mcp-type"
                className="mb-1 block text-xs font-medium"
              >
                Transport
              </label>
              <select
                id="mcp-type"
                value={draft.type}
                onChange={(e) =>
                  setDraft({
                    ...draft,
                    type: e.target.value as DraftEntry["type"],
                  })
                }
                className="h-8 w-full rounded-md border border-input bg-background px-2 text-sm"
                aria-label="MCP transport"
              >
                <option value="stdio">stdio (local subprocess)</option>
                <option value="http">http (remote)</option>
                <option value="sse">sse (remote)</option>
              </select>
              {draft.type !== "stdio" && (
                <p className="mt-1 text-[11px] text-muted-foreground">
                  Note: Codex&apos;s `-c` config does not accept remote MCPs;
                  this entry will be skipped for Codex sessions only.
                </p>
              )}
            </div>

            {draft.type === "stdio" ? (
              <>
                <div>
                  <label
                    htmlFor="mcp-command"
                    className="mb-1 block text-xs font-medium"
                  >
                    Command
                  </label>
                  <Input
                    id="mcp-command"
                    value={draft.command}
                    onChange={(e) =>
                      setDraft({ ...draft, command: e.target.value })
                    }
                    placeholder="/usr/local/bin/mcp-server-sqlite"
                    className="font-mono text-xs"
                  />
                </div>
                <div>
                  <label
                    htmlFor="mcp-args"
                    className="mb-1 block text-xs font-medium"
                  >
                    Arguments (space-separated)
                  </label>
                  <Input
                    id="mcp-args"
                    value={draft.args}
                    onChange={(e) =>
                      setDraft({ ...draft, args: e.target.value })
                    }
                    placeholder="--db-path /tmp/x.db"
                    className="font-mono text-xs"
                  />
                </div>
                <div>
                  <label
                    htmlFor="mcp-env"
                    className="mb-1 block text-xs font-medium"
                  >
                    Environment variables (one KEY=VALUE per line, optional)
                  </label>
                  <Textarea
                    id="mcp-env"
                    value={draft.envText}
                    onChange={(e) =>
                      setDraft({ ...draft, envText: e.target.value })
                    }
                    placeholder="API_TOKEN=…"
                    rows={3}
                    className="font-mono text-xs"
                  />
                </div>
              </>
            ) : (
              <div>
                <label
                  htmlFor="mcp-url"
                  className="mb-1 block text-xs font-medium"
                >
                  URL
                </label>
                <Input
                  id="mcp-url"
                  value={draft.url}
                  onChange={(e) =>
                    setDraft({ ...draft, url: e.target.value })
                  }
                  placeholder="https://mcp.example.com/v1"
                  className="font-mono text-xs"
                />
              </div>
            )}

            {draftError && (
              <p className="text-xs text-destructive">{draftError}</p>
            )}
          </div>

          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setDialogOpen(false)}
              disabled={saving}
            >
              Cancel
            </Button>
            <Button onClick={() => void handleSave()} disabled={saving}>
              {saving ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function McpServerRow({
  name,
  cfg,
  onEdit,
  onDelete,
  disabled,
}: {
  name: string;
  cfg: McpServerConfig;
  onEdit: () => void;
  onDelete: () => void;
  disabled: boolean;
}) {
  const subtitle =
    cfg.type === "stdio"
      ? `${cfg.command ?? ""}${cfg.args && cfg.args.length > 0 ? " " + cfg.args.join(" ") : ""}`
      : (cfg.url ?? "");

  return (
    <div className="flex items-center gap-3 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm font-medium">
          <span className="truncate">{name}</span>
          <span className="rounded-sm border border-border px-1 py-0.5 text-[10px] uppercase text-muted-foreground">
            {cfg.type}
          </span>
        </div>
        <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground">
          {subtitle || "—"}
        </div>
      </div>
      <Button
        variant="ghost"
        size="sm"
        onClick={onEdit}
        disabled={disabled}
        aria-label={`Edit ${name}`}
      >
        <Pencil className="h-3.5 w-3.5" />
      </Button>
      <Button
        variant="ghost"
        size="sm"
        onClick={onDelete}
        disabled={disabled}
        aria-label={`Delete ${name}`}
      >
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </div>
  );
}
