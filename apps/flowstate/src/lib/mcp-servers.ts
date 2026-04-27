// User-defined global MCP server registry. Backed by the
// canonical file `~/.flowstate/mcp.json` — Rust adapters load it on
// session spawn and merge entries into every provider's native MCP
// channel. The Settings UI mirrors the same file via two Tauri
// commands defined in `apps/flowstate/src-tauri/src/lib.rs`:
// `get_user_mcp_servers` / `set_user_mcp_servers`.
//
// The schema mirrors `zenui_provider_api::McpServerConfig` /
// `McpConfigFile`. Defining it locally rather than auto-generating
// from Rust (the way `ProviderKind` etc. are) keeps the surface
// small — these types are only used by the Settings panel.

import { invoke } from "@tauri-apps/api/core";

/** Reserved entry name owned by the flowstate orchestration server.
 *  Stripped server-side before write; surfaced here so the UI can
 *  reject the name client-side with a friendlier message. */
export const RESERVED_MCP_NAME = "flowstate";

/** Wire shape mirrored from Rust. `command` populated only for
 *  stdio; `url` populated only for http/sse. The Tauri command on
 *  the Rust side validates these invariants before writing. */
export interface McpServerConfig {
  /** Wire-named `type` because every MCP-aware provider parses it
   *  under that key. The TS field renames to keep our code idiomatic. */
  type: "stdio" | "http" | "sse";
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
}

export interface McpConfigFile {
  mcpServers: Record<string, McpServerConfig>;
}

/** Read the current contents of `~/.flowstate/mcp.json`. Missing /
 *  invalid file resolves to `{ mcpServers: {} }`. */
export function getMcpServers(): Promise<McpConfigFile> {
  return invoke<McpConfigFile>("get_user_mcp_servers");
}

/** Atomically replace `~/.flowstate/mcp.json`. Reserved key is
 *  stripped server-side; per-entry validation runs before any write
 *  hits disk, so an invalid entry rejects the whole update. The
 *  resolved value is the new on-disk contents. */
export function setMcpServers(
  servers: Record<string, McpServerConfig>,
): Promise<McpConfigFile> {
  return invoke<McpConfigFile>("set_user_mcp_servers", { servers });
}

/** Lightweight client-side validation, mirroring
 *  `validate_mcp_server_config` on the Rust side. The server is the
 *  source of truth — this just shortens the round-trip on obvious
 *  errors so the UI can flag inline. Returns `null` when valid. */
export function validateMcpEntry(
  name: string,
  cfg: McpServerConfig,
): string | null {
  if (!name.trim()) return "Name is required";
  if (name === RESERVED_MCP_NAME)
    return `"${RESERVED_MCP_NAME}" is reserved by flowstate`;
  if (cfg.type === "stdio") {
    if (!cfg.command || !cfg.command.trim())
      return "stdio servers require a command";
  } else if (cfg.type === "http" || cfg.type === "sse") {
    if (!cfg.url || !cfg.url.trim())
      return `${cfg.type} servers require a URL`;
  } else {
    return `Unknown transport ${String(cfg.type)}`;
  }
  return null;
}
