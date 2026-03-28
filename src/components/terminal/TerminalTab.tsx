import * as React from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { UnicodeGraphemesAddon } from "@xterm/addon-unicode-graphemes";
import {
  killPty,
  openPty,
  pausePty,
  resizePty,
  resumePty,
  writePty,
  type PtyId,
} from "@/lib/api";

interface TerminalTabProps {
  /** Stable tab id from the store — used only for keying. */
  tabId: string;
  /** Absolute cwd the shell should be started in. */
  cwd: string;
  /** True when this is the active tab for the current project. */
  isVisible: boolean;
  /** Dispatches tab-title updates when the shell sends OSC 2. */
  onTitleChange: (title: string) => void;
  /** Called when the tab's shell exits (EOF on the channel). The
   *  dock uses this to auto-close the tab. */
  onExit: () => void;
}

// xterm.js write-flow-control watermarks. The reader thread on the
// Rust side keeps pumping bytes; when the frontend can't keep up
// (pending callbacks cross HIGH) we pause the reader, and when
// they fall below LOW we resume. Ack every CHUNK_BYTES rather than
// every write() so quiet shells don't thrash the IPC.
const HIGH_WATERMARK = 5;
const LOW_WATERMARK = 2;
const CHUNK_BYTES = 100_000;

export function TerminalTab({
  cwd,
  isVisible,
  onTitleChange,
  onExit,
}: TerminalTabProps) {
  const hostRef = React.useRef<HTMLDivElement>(null);
  const termRef = React.useRef<Terminal | null>(null);
  const fitRef = React.useRef<FitAddon | null>(null);
  const webglRef = React.useRef<WebglAddon | null>(null);
  const ptyIdRef = React.useRef<PtyId | null>(null);
  const onTitleChangeRef = React.useRef(onTitleChange);
  const onExitRef = React.useRef(onExit);

  React.useEffect(() => {
    onTitleChangeRef.current = onTitleChange;
  }, [onTitleChange]);
  React.useEffect(() => {
    onExitRef.current = onExit;
  }, [onExit]);

  // One-time setup: create the xterm instance, open it on the host
  // div, spawn the pty, wire I/O, dispose on unmount. WebGL is
  // attached separately in the visibility effect below.
  React.useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const term = new Terminal({
      cursorBlink: true,
      cursorStyle: "bar",
      fontFamily:
        '"JetBrains Mono", "Geist Mono", ui-monospace, Menlo, Consolas, monospace',
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 5000,
      allowTransparency: false,
      theme: {
        background: "#0f0f10",
        foreground: "#e5e5e6",
        cursor: "#e5e5e6",
        cursorAccent: "#0f0f10",
        selectionBackground: "#3a3a3d",
      },
    });
    termRef.current = term;

    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    term.loadAddon(new WebLinksAddon());
    try {
      term.loadAddon(new UnicodeGraphemesAddon());
    } catch {
      // experimental addon, non-fatal if it throws
    }

    term.open(host);

    let disposed = false;
    let pending = 0;
    let acc = 0;

    // First fit happens on the next frame so the host has been
    // measured by the layout pass. Only then do we have real
    // cols/rows to hand to the pty.
    const rafId = requestAnimationFrame(async () => {
      if (disposed) return;
      try {
        fit.fit();
      } catch {
        // host might be zero-sized (dock still animating in)
      }
      const cols = term.cols || 80;
      const rows = term.rows || 24;
      try {
        const id = await openPty({
          cols,
          rows,
          cwd,
          onData: (bytes) => {
            const t = termRef.current;
            if (!t) return;
            const buf = new Uint8Array(bytes);
            acc += buf.byteLength;
            const needAck = acc >= CHUNK_BYTES;
            if (needAck) {
              acc = 0;
              pending++;
            }
            t.write(
              buf,
              needAck
                ? () => {
                    pending--;
                    if (pending < LOW_WATERMARK && ptyIdRef.current != null) {
                      resumePty(ptyIdRef.current).catch(() => {});
                    }
                  }
                : undefined,
            );
            if (pending > HIGH_WATERMARK && ptyIdRef.current != null) {
              pausePty(ptyIdRef.current).catch(() => {});
            }
          },
        });
        if (disposed) {
          await killPty(id);
          return;
        }
        ptyIdRef.current = id;
      } catch (e) {
        if (!disposed) {
          term.writeln("");
          term.writeln(`\x1b[31mfailed to start shell: ${String(e)}\x1b[0m`);
          onExitRef.current();
        }
      }
    });

    // Stdin: xterm -> pty
    const encoder = new TextEncoder();
    const inputDisp = term.onData((data) => {
      const id = ptyIdRef.current;
      if (id != null) {
        writePty(id, encoder.encode(data)).catch(() => {});
      }
    });

    // Grid changes (fit computes new cols/rows) -> SIGWINCH to child
    const resizeDisp = term.onResize(({ cols, rows }) => {
      const id = ptyIdRef.current;
      if (id != null) {
        resizePty(id, cols, rows).catch(() => {});
      }
    });

    // OSC 2 title updates from the shell (e.g. zsh prompt setting it)
    const titleDisp = term.onTitleChange((title) => {
      onTitleChangeRef.current(title);
    });

    // Debounced refit on container resize. rAF-coalesced so we don't
    // thrash the fit algorithm during a drag.
    let pendingRaf: number | null = null;
    const observer = new ResizeObserver(() => {
      if (pendingRaf != null) cancelAnimationFrame(pendingRaf);
      pendingRaf = requestAnimationFrame(() => {
        pendingRaf = null;
        try {
          fit.fit();
        } catch {
          // ignore — may fire with zero size during hide animation
        }
      });
    });
    observer.observe(host);

    return () => {
      disposed = true;
      cancelAnimationFrame(rafId);
      if (pendingRaf != null) cancelAnimationFrame(pendingRaf);
      observer.disconnect();
      inputDisp.dispose();
      resizeDisp.dispose();
      titleDisp.dispose();
      const id = ptyIdRef.current;
      if (id != null) {
        killPty(id).catch(() => {});
      }
      webglRef.current?.dispose();
      webglRef.current = null;
      term.dispose();
      termRef.current = null;
    };
  }, [cwd]);

  // Attach/dispose the WebGL renderer when visibility flips. Hidden
  // tabs drop the GPU context to free texture atlas memory; the
  // xterm buffer and pty reader keep running so commands in the
  // background are not paused. Recreating on show (rather than
  // keeping a persistent canvas through the display:none cycle)
  // avoids a one-frame "big text" flash where the stale canvas
  // paints at its last size before fit runs.
  React.useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    if (isVisible) {
      if (!webglRef.current) {
        try {
          const webgl = new WebglAddon();
          webgl.onContextLoss(() => {
            webgl.dispose();
            webglRef.current = null;
          });
          term.loadAddon(webgl);
          webglRef.current = webgl;
        } catch {
          // webgl2 unavailable — fall back to the DOM renderer
          // silently. The terminal still works.
        }
      }
      const handle = requestAnimationFrame(() => {
        try {
          fitRef.current?.fit();
        } catch {
          // ignore
        }
        term.focus();
      });
      return () => cancelAnimationFrame(handle);
    } else {
      webglRef.current?.dispose();
      webglRef.current = null;
      return undefined;
    }
  }, [isVisible]);

  return (
    <div
      ref={hostRef}
      className="h-full w-full"
      style={{ display: isVisible ? "block" : "none" }}
    />
  );
}
