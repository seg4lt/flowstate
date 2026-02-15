import { invoke } from "@tauri-apps/api/core";

export async function getDaemonUrl(): Promise<string> {
  return invoke<string>("get_daemon_url");
}
