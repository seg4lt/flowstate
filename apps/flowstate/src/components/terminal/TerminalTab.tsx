import * as React from "react";
import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { UnicodeGraphemesAddon } from "@xterm/addon-unicode-graphemes";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  killPty,
  openPty,
  pausePty,
  resizePty,
  resumePty,
  writePty,
  type PtyId,
} from "@/lib/api";
import { useTheme } from "@/hooks/use-theme";

// Hex equivalents of the OKLCH tokens in src/index.css. Kept here as
// concrete color strings because xterm.js does not re-resolve CSS
// variables — it parses the values once and feeds them to the WebGL
// texture atlas / DOM renderer.
const TERMINAL_THEMES: Record<"light" | "dark", ITheme> = {
  dark: {
    background: "#252525", // sRGB equivalent of oklch(0.145 0 0) = --background.dark
    foreground: "#fafafa", // ~ oklch(0.985 0 0)
    cursor: "#fafafa",
    cursorAccent: "#252525",
    selectionBackground: "#3a3a3d",
  },
  light: {
    background: "#ffffff", // oklch(1 0 0)
    foreground: "#252525", // ~ oklch(0.145 0 0)
    cursor: "#252525",
    cursorAccent: "#ffffff",
    selectionBackground: "#d4d4d4", // ~ oklch(0.85 0 0), readable on white
  },
};

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

  const { resolvedTheme } = useTheme();
  // Track the active theme via a ref so the mount effect can read the
  // current value at construction time without taking a dependency on it
  // (which would tear down the PTY on every theme flip).
  const themeRef = React.useRef(resolvedTheme);
  themeRef.current = resolvedTheme;

  React.useEffect(() => {
    onTitleChangeRef.current = onTitleChange;
  }, [onTitleChange]);
  React.useEffect(() => {
    onExitRef.current = onExit;
  }, [onExit]);

  // Live-update the xterm theme when the app theme flips. Setting
  // `term.options.theme` is the supported xterm.js v6 path for runtime
  // theme changes — both the WebGL renderer and the DOM fallback re-paint
  // on the next frame.
  React.useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.theme = TERMINAL_THEMES[resolvedTheme];
  }, [resolvedTheme]);

  // Refit only when the host actually has non-zero dimensions. When the
  // OS minimises the window the host <div> collapses to ~0×0 and a naive
  // `fit.fit()` would compute a tiny cols/rows, push it through
  // `term.onResize → resizePty` (SIGWINCH the child at the bogus size),
  // and corrupt the buffer on restore. Skipping zero-sized fits leaves
  // the previous good size in place until the host has real dimensions
  // again.
  const safeFit = React.useCallback(() => {
    const host = hostRef.current;
    const fit = fitRef.current;
    if (!host || !fit) return;
    if (!host.isConnected) return;
    if (host.clientWidth <= 0 || host.clientHeight <= 0) return;
    try {
      fit.fit();
    } catch {
      // FitAddon can still throw if a measurement races with layout.
    }
  }, []);

  // One-time setup: create the xterm instance, open it on the host
  // div, spawn the pty, wire I/O, dispose on unmount. WebGL is
  // attached separately in the visibility effect below.
  React.useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const term = new Terminal({
      cursorBlink: true,
      cursorStyle: "bar",
      macOptionIsMeta: true,
      fontFamily:
        '"JetBrains Mono", "Geist Mono", ui-monospace, Menlo, Consolas, monospace',
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 5000,
      allowTransparency: false,
      theme: TERMINAL_THEMES[themeRef.current],
    });
    termRef.current = term;

    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    // xterm's default WebLinksAddon handler calls
    // `window.open(url, "_blank")`, which Tauri's WKWebView silently
    // drops — links appear underlined but clicking does nothing.
    // Route through `openUrl` from `@tauri-apps/plugin-opener` (the
    // same path `markdown-content.tsx` uses) so a click on a URL in
    // shell output opens the system browser. Falls back to
    // `window.open` outside Tauri so the addon still works in a
    // plain-browser dev/test build.
    term.loadAddon(
      new WebLinksAddon((_event, url) => {
        if ("__TAURI_INTERNALS__" in window) {
          void openUrl(url);
        } else {
          window.open(url, "_blank", "noopener,noreferrer");
        }
      }),
    );
    try {
      term.loadAddon(new UnicodeGraphemesAddon());
    } catch {
      // experimental addon, non-fatal if it throws
    }

    term.open(host);

    // Mac natural text navigation — translate common macOS editing
    // shortcuts into the escape sequences shells understand.
    const enc = new TextEncoder();
    term.attachCustomKeyEventHandler((ev) => {
      if (ev.type !== "keydown") return true;
      const id = ptyIdRef.current;
      if (id == null) return true;

      // Option (Alt) + Arrow / Backspace
      if (ev.altKey && !ev.metaKey && !ev.ctrlKey) {
        switch (ev.key) {
          case "ArrowLeft":
            writePty(id, enc.encode("\x1bb")).catch(() => {});
            return false;
          case "ArrowRight":
            writePty(id, enc.encode("\x1bf")).catch(() => {});
            return false;
          case "Backspace":
            writePty(id, enc.encode("\x1b\x7f")).catch(() => {});
            return false;
        }
      }

      // Cmd (Meta) + Arrow / Backspace
      if (ev.metaKey && !ev.altKey && !ev.ctrlKey) {
        switch (ev.key) {
          case "ArrowLeft":
            writePty(id, enc.encode("\x01")).catch(() => {});
            return false;
          case "ArrowRight":
            writePty(id, enc.encode("\x05")).catch(() => {});
            return false;
          case "Backspace":
            writePty(id, enc.encode("\x15")).catch(() => {});
            return false;
        }
      }

      return true;
    });

    let disposed = false;
    let pending = 0;
    let acc = 0;

    // First fit happens on the next frame so the host has been
    // measured by the layout pass. Only then do we have real
    // cols/rows to hand to the pty.
    const rafId = requestAnimationFrame(async () => {
      if (disposed) return;
      safeFit();
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
          // The Rust reader thread sends a single `Exit` event when
          // the shell process dies — clean `exit`, signal, or kill
          // from another thread. Bubble that up to the dock so the
          // tab auto-closes. The `disposed` guard skips this when
          // React has already torn the component down (e.g. the
          // user clicked the X button, which calls killPty → EOF →
          // Exit, but the unmount cleanup has already run).
          onExit: () => {
            if (disposed) return;
            onExitRef.current();
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
    const inputDisp = term.onData((data) => {
      const id = ptyIdRef.current;
      if (id != null) {
        writePty(id, enc.encode(data)).catch(() => {});
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
        // safeFit skips firings where the host is hidden / 0×0 (e.g.
        // during the OS minimise → restore animation), which would
        // otherwise SIGWINCH the PTY at a bogus size and corrupt the
        // visible buffer on restore.
        safeFit();
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
  }, [cwd, safeFit]);

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
        safeFit();
        term.focus();
      });
      return () => cancelAnimationFrame(handle);
    } else {
      webglRef.current?.dispose();
      webglRef.current = null;
      return undefined;
    }
  }, [isVisible, safeFit]);

  return (
    <div
      ref={hostRef}
      className="h-full w-full"
      style={{ display: isVisible ? "block" : "none" }}
    />
  );
}
