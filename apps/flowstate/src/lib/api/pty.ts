import { Channel, invoke } from "@tauri-apps/api/core";

// Integrated terminal — PTY control plane. Frontend pairs this
// with @xterm/xterm on the render side. `openPty` creates a shell
// child and returns a numeric id; the provided onData channel
// delivers the shell's raw byte output (as a number array today;
// upgradeable to ArrayBuffer when we care). All the other helpers
// take that id as the first arg.
export type PtyId = number;

export interface OpenPtyOptions {
  cols: number;
  rows: number;
  cwd?: string;
  shell?: string;
  onData: (bytes: number[]) => void;
}

export function openPty(opts: OpenPtyOptions): Promise<PtyId> {
  const channel = new Channel<number[]>();
  channel.onmessage = opts.onData;
  return invoke<PtyId>("pty_open", {
    cols: opts.cols,
    rows: opts.rows,
    cwd: opts.cwd ?? null,
    shell: opts.shell ?? null,
    onData: channel,
  });
}

export function writePty(id: PtyId, data: Uint8Array): Promise<void> {
  return invoke<void>("pty_write", { id, data: Array.from(data) });
}

export function resizePty(
  id: PtyId,
  cols: number,
  rows: number,
): Promise<void> {
  return invoke<void>("pty_resize", { id, cols, rows });
}

export function pausePty(id: PtyId): Promise<void> {
  return invoke<void>("pty_pause", { id });
}

export function resumePty(id: PtyId): Promise<void> {
  return invoke<void>("pty_resume", { id });
}

export function killPty(id: PtyId): Promise<void> {
  return invoke<void>("pty_kill", { id });
}
