import * as React from "react";
import {
  Code2,
  Copy,
  ExternalLink,
  FileText,
  GitBranch,
  GitCompare,
  Globe,
  History,
  Keyboard,
  MessageSquare,
  Search,
  Sparkles,
  TerminalSquare,
  Wand2,
  Zap,
} from "lucide-react";
import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { isMacOS } from "@/lib/popout";
import { cn } from "@/lib/utils";

// Features page. A bespoke marketing-style overview of what Flowstate
// brings beyond a stock chat surface: the MCP that lets agents talk
// to each other, the in-app editor / diff view / markdown / html
// preview, fuzzy search, copy, and the popout window. Rendered as a
// single-route page (no data fetching) so the visual identity here
// can be richer than the rest of the app — gradients, glow rings,
// big numbers — without leaking style noise into the chat surface.
//
// Light/dark is handled by referencing only Tailwind theme variables
// (--background, --foreground, --card, --border, --muted-foreground,
// etc.) plus a couple of channel-aware accents that keep enough
// contrast in both modes. Nothing here hardcodes a color literal
// outside of the gradient stops, which use opacity stops on the
// existing theme channels.

type FeatureCardProps = {
  icon: React.ReactNode;
  title: string;
  blurb: string;
  bullets: string[];
  accent: string;
  badge?: string;
};

function FeatureCard({
  icon,
  title,
  blurb,
  bullets,
  accent,
  badge,
}: FeatureCardProps) {
  return (
    <div
      className={cn(
        "group relative flex flex-col overflow-hidden rounded-2xl border border-border/60 bg-card p-5 transition-all duration-300",
        "hover:-translate-y-0.5 hover:border-border hover:shadow-[0_8px_30px_-12px_rgba(0,0,0,0.25)] dark:hover:shadow-[0_8px_30px_-12px_rgba(0,0,0,0.65)]",
      )}
    >
      {/* Soft top-left glow that pulls a per-card accent into the
          surface without hardcoding a theme-incompatible bg color.
          Sits behind the content via a negative z so it never
          intercepts pointer events. */}
      <div
        aria-hidden="true"
        className="pointer-events-none absolute -left-12 -top-12 h-40 w-40 rounded-full opacity-50 blur-3xl transition-opacity duration-500 group-hover:opacity-80"
        style={{ background: accent }}
      />
      <div className="relative flex items-start justify-between">
        <div
          className={cn(
            "flex h-10 w-10 items-center justify-center rounded-xl border border-border/60 bg-background/80 backdrop-blur",
            "shadow-sm",
          )}
          style={{ color: accent }}
        >
          {icon}
        </div>
        {badge ? (
          <span className="rounded-full border border-border/70 bg-background/70 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
            {badge}
          </span>
        ) : null}
      </div>
      <div className="relative mt-4">
        <h3 className="text-base font-semibold tracking-tight text-foreground">
          {title}
        </h3>
        <p className="mt-1.5 text-[13px] leading-relaxed text-muted-foreground">
          {blurb}
        </p>
      </div>
      <ul className="relative mt-4 space-y-1.5">
        {bullets.map((b) => (
          <li
            key={b}
            className="flex items-start gap-2 text-[12.5px] leading-relaxed text-foreground/80"
          >
            <span
              aria-hidden="true"
              className="mt-1.5 h-1 w-1 shrink-0 rounded-full"
              style={{ background: accent }}
            />
            <span>{b}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function CodeBlock({
  language,
  children,
}: {
  language: string;
  children: string;
}) {
  const [copied, setCopied] = React.useState(false);
  const onCopy = React.useCallback(() => {
    void navigator.clipboard.writeText(children).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    });
  }, [children]);
  return (
    <div className="group relative overflow-hidden rounded-xl border border-border/60 bg-muted/40">
      <div className="flex items-center justify-between border-b border-border/50 bg-background/40 px-3 py-1.5">
        <span className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
          {language}
        </span>
        <button
          type="button"
          onClick={onCopy}
          className="inline-flex items-center gap-1.5 rounded-md border border-transparent px-1.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:border-border hover:bg-background hover:text-foreground"
        >
          <Copy className="h-3 w-3" />
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre className="overflow-x-auto p-3 text-[12.5px] leading-relaxed text-foreground/90">
        <code>{children}</code>
      </pre>
    </div>
  );
}

function FlowstateMcpSection() {
  return (
    <section className="relative overflow-hidden rounded-2xl border border-border/60 bg-card p-6 sm:p-8">
      {/* Two diffuse accent washes — one warm, one cool — so the
          MCP feature reads as the marquee item without a literal
          colored bg that breaks under the dark theme. */}
      <div
        aria-hidden="true"
        className="pointer-events-none absolute -right-24 -top-24 h-72 w-72 rounded-full bg-[oklch(0.72_0.18_290)/0.18] blur-3xl"
      />
      <div
        aria-hidden="true"
        className="pointer-events-none absolute -bottom-24 -left-24 h-72 w-72 rounded-full bg-[oklch(0.78_0.15_200)/0.18] blur-3xl"
      />
      <div className="relative grid gap-6 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)] lg:gap-10">
        <div>
          <div className="inline-flex items-center gap-2 rounded-full border border-border/70 bg-background/60 px-2.5 py-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground backdrop-blur">
            <Sparkles className="h-3 w-3" />
            Bespoke
          </div>
          <h2 className="mt-3 text-2xl font-semibold tracking-tight text-foreground sm:text-3xl">
            Flowstate MCP
          </h2>
          <p className="mt-3 text-sm leading-relaxed text-muted-foreground sm:text-[15px]">
            A first-class Model Context Protocol server baked into
            Flowstate. Your agent can spawn other sessions, send them
            messages, read transcripts, and provision isolated git
            worktrees — every running thread becomes a coworker the
            current agent can delegate to.
          </p>
          <ul className="mt-5 grid gap-2 text-[13px] text-foreground/85 sm:grid-cols-2">
            <li className="flex items-start gap-2">
              <span className="mt-1.5 h-1 w-1 shrink-0 rounded-full bg-foreground/60" />
              <span>
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  spawn
                </code>{" "}
                /{" "}
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  spawn_and_await
                </code>{" "}
                a peer
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span className="mt-1.5 h-1 w-1 shrink-0 rounded-full bg-foreground/60" />
              <span>
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  send
                </code>{" "}
                /{" "}
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  send_and_await
                </code>{" "}
                between sessions
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span className="mt-1.5 h-1 w-1 shrink-0 rounded-full bg-foreground/60" />
              <span>
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  create_worktree
                </code>{" "}
                +{" "}
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  spawn_in_worktree
                </code>
              </span>
            </li>
            <li className="flex items-start gap-2">
              <span className="mt-1.5 h-1 w-1 shrink-0 rounded-full bg-foreground/60" />
              <span>
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  read_session
                </code>{" "}
                /{" "}
                <code className="rounded bg-muted px-1 py-px text-[12px]">
                  poll
                </code>{" "}
                across providers
              </span>
            </li>
          </ul>
        </div>
        <div className="space-y-3">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
            Try it — paste into any thread
          </div>
          <CodeBlock language="prompt">{`Use the flowstate mcp to spin up a worktree off
main called fix/login-flow, have a Codex agent
rewrite the auth handler there, and a Claude agent
write tests in parallel.

When both reply, summarize their diffs and pick
the one with better error handling.`}</CodeBlock>
          <CodeBlock language="prompt">{`Use the flowstate mcp: list my running sessions,
find the one working on the migration script,
send it "switch to a transactional approach and
re-run the dry run", then poll for its reply.`}</CodeBlock>
          <CodeBlock language="prompt">{`Use the flowstate mcp to spawn an agent on the
acme-web project and discuss how their feature-flag
system is implemented. Once you understand it,
coordinate with the peer on how we can reuse the
same pattern here — agree on an interface, then
have it draft the port while you review.`}</CodeBlock>
        </div>
      </div>
    </section>
  );
}

const FEATURES: FeatureCardProps[] = [
  {
    icon: <Code2 className="h-5 w-5" />,
    title: "Editor",
    blurb:
      "An in-app code editor for opening, editing, and reviewing files without leaving the thread.",
    bullets: [
      "Tabs, syntax highlighting, and ⌘W to close",
      "⌘P to jump to any file by name",
      "Edits stay scoped to the project's git worktree",
    ],
    accent: "oklch(0.72 0.16 250 / 0.45)",
  },
  {
    icon: <Keyboard className="h-5 w-5" />,
    title: "Vim Mode",
    blurb:
      "Full Vim keybindings inside the editor — Normal, Insert, Visual, the works — toggled on with one click.",
    bullets: [
      "Flip on in Settings → Appearance, applies to every open tab",
      "Live --VISUAL-- / --INSERT-- mode indicator at the bottom",
      "Survives reloads and coexists with the app's own shortcuts",
    ],
    accent: "oklch(0.74 0.14 140 / 0.45)",
  },
  {
    icon: <GitCompare className="h-5 w-5" />,
    title: "Diff View",
    blurb:
      "A side-by-side diff for every change the agent proposes — review hunks before they hit disk.",
    bullets: [
      "Inline and split layouts toggle on the fly",
      "Hunk-level accept / reject when you want a closer look",
      "Stays in sync with the worktree as the agent iterates",
    ],
    accent: "oklch(0.78 0.16 150 / 0.45)",
  },
  {
    icon: <MessageSquare className="h-5 w-5" />,
    title: "Inline Comments",
    blurb:
      "Hover any line in a diff or an open file and drop a comment — anchored to the exact path and range — before sending to the agent.",
    bullets: [
      "Hover gutter for a + affordance, or ⌥⌘C on a selection",
      "Single-line or multi-line ranges, with the source captured",
      "Stacks as chips above the composer until you actually send",
    ],
    accent: "oklch(0.78 0.14 100 / 0.45)",
  },
  {
    icon: <History className="h-5 w-5" />,
    title: "Checkpoints",
    blurb:
      "Every turn snapshots the files the agent touched, so you can rewind the workspace to any prior message in the thread.",
    bullets: [
      "Dry-run preview shows exactly what restore / delete / skip will do",
      "Detects out-of-band edits and asks before overwriting",
      "Rewind to any message — the conversation stays put, the files snap back",
    ],
    accent: "oklch(0.78 0.15 50 / 0.45)",
  },
  {
    icon: <GitBranch className="h-5 w-5" />,
    title: "Worktrees",
    blurb:
      "First-class git worktrees: spin one up from the new-thread dropdown and run an agent against an isolated branch in seconds.",
    bullets: [
      "Create a fresh branch or check out an existing one inline",
      "Each worktree shows up as its own project, with its own sessions",
      "Pairs with the MCP so peer agents can work in parallel branches",
    ],
    accent: "oklch(0.74 0.16 170 / 0.45)",
  },
  {
    icon: <TerminalSquare className="h-5 w-5" />,
    title: "Terminal",
    blurb:
      "An embedded xterm.js terminal docked under the chat, so you can run a build or tail logs without leaving the thread.",
    bullets: [
      "Multiple tabs per project, each with its own live PTY",
      "Resizable dock that remembers its height per session",
      "Shells stay alive across thread switches and minimizes",
    ],
    accent: "oklch(0.72 0.13 260 / 0.45)",
  },
  {
    icon: <FileText className="h-5 w-5" />,
    title: "Markdown",
    blurb:
      "Rich markdown rendering for messages, with code fences, tables, callouts, and task lists.",
    bullets: [
      "Live preview while the agent is still streaming",
      "GFM tables and checkbox lists out of the box",
      "Per-block copy buttons for snippets",
    ],
    accent: "oklch(0.78 0.14 70 / 0.45)",
  },
  {
    icon: <Globe className="h-5 w-5" />,
    title: "HTML Preview",
    blurb:
      "Render an HTML response in a sandboxed preview right beside the chat — perfect for prototypes.",
    bullets: [
      "Iframe-isolated so scripts can't escape the preview",
      "Toggle between source and rendered output",
      "Light and dark backgrounds match the app theme",
    ],
    accent: "oklch(0.74 0.16 320 / 0.45)",
    badge: "New",
  },
  {
    icon: <Search className="h-5 w-5" />,
    title: "Fuzzy Search",
    blurb:
      "Find anything — files, threads, projects — by typing a few characters. Built for speed.",
    bullets: [
      "⌘P for files, ⌘⇧F for content across the project",
      "Tolerant of typos and out-of-order matches",
      "Recent results stay near the top",
    ],
    accent: "oklch(0.78 0.13 230 / 0.45)",
  },
  {
    icon: <Copy className="h-5 w-5" />,
    title: "Copy",
    blurb:
      "Copy any message, code block, diff hunk, or tool output with a single click.",
    bullets: [
      "Per-block hover affordance — never copies more than you meant",
      "Plain-text copy for code, rich copy for prose",
      "Subtle confirmation so you know it landed",
    ],
    accent: "oklch(0.74 0.13 30 / 0.45)",
  },
  {
    icon: <ExternalLink className="h-5 w-5" />,
    title: "Popout",
    blurb:
      "Pop a thread into its own window so you can babysit a long-running agent while you keep working.",
    bullets: [
      "Stays on top with ⌘⇧T",
      "Minimal chrome — just the thread, the composer, the preview",
      "Multiple popouts run independently in parallel",
    ],
    accent: "oklch(0.74 0.16 0 / 0.45)",
  },
  {
    icon: <Wand2 className="h-5 w-5" />,
    title: "Provider Routing",
    blurb:
      "Pick the right model per thread — Claude, Codex, GitHub Copilot, OpenCode — without losing context.",
    bullets: [
      "Per-project default provider with per-thread override",
      "Reasoning effort and permission mode are first-class",
      "Drop-in worktrees keep edits isolated",
    ],
    accent: "oklch(0.74 0.18 280 / 0.45)",
  },
];

export function FeaturesView() {
  const { state: sidebarState } = useSidebar();
  const showMacTrafficSpacer = isMacOS() && sidebarState === "collapsed";

  return (
    <div className="flex h-full flex-col">
      <header
        data-tauri-drag-region
        className="flex h-9 items-center gap-1 border-b border-border px-2"
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        <SidebarTrigger />
        <div className="flex-1 text-sm font-medium">Features</div>
      </header>

      <div className="flex-1 overflow-auto">
        {/* Hero. Layered radial washes give the page its own
            visual identity without leaking into chat. The grid
            overlay is a single SVG so it scales cleanly under
            zoom shortcuts (⌘+/⌘-/⌘0 from useZoomShortcuts). */}
        <section className="relative overflow-hidden border-b border-border/60">
          <div
            aria-hidden="true"
            className="pointer-events-none absolute inset-0 opacity-[0.55] dark:opacity-[0.35]"
            style={{
              backgroundImage:
                "radial-gradient(60% 80% at 20% 10%, oklch(0.78 0.14 280 / 0.18) 0%, transparent 60%), radial-gradient(50% 70% at 80% 0%, oklch(0.78 0.14 200 / 0.16) 0%, transparent 60%), radial-gradient(40% 60% at 90% 90%, oklch(0.78 0.14 150 / 0.14) 0%, transparent 65%)",
            }}
          />
          <div
            aria-hidden="true"
            className="pointer-events-none absolute inset-0 opacity-[0.06] [mask-image:radial-gradient(ellipse_at_center,black_30%,transparent_75%)]"
            style={{
              backgroundImage:
                "linear-gradient(to right, currentColor 1px, transparent 1px), linear-gradient(to bottom, currentColor 1px, transparent 1px)",
              backgroundSize: "32px 32px",
            }}
          />
          <div className="relative mx-auto flex max-w-5xl flex-col items-start px-6 py-14 sm:py-20">
            <div className="inline-flex items-center gap-2 rounded-full border border-border/70 bg-background/60 px-3 py-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground backdrop-blur">
              <Zap className="h-3 w-3" />
              What's inside Flowstate
            </div>
            <h1 className="mt-4 max-w-3xl text-3xl font-semibold tracking-tight text-foreground sm:text-4xl">
              The features that make{" "}
              <span className="bg-gradient-to-r from-foreground to-foreground/60 bg-clip-text text-transparent">
                day-to-day agent work
              </span>{" "}
              feel native.
            </h1>
            <p className="mt-3 max-w-2xl text-[15px] leading-relaxed text-muted-foreground">
              Flowstate isn't just a chat surface. It's an editor, a diff
              viewer, a markdown and HTML preview, a fuzzy search, a popout
              window, and an MCP that lets your agents collaborate. Browse
              the highlights below.
            </p>
          </div>
        </section>

        <div className="mx-auto max-w-5xl space-y-6 px-6 py-8">
          {/* Marquee — Flowstate MCP gets its own row above the grid. */}
          <FlowstateMcpSection />

          <div className="flex items-end justify-between pt-2">
            <h2 className="text-lg font-semibold tracking-tight text-foreground">
              Everything else, at a glance
            </h2>
            <span className="text-xs text-muted-foreground">
              {FEATURES.length} features
            </span>
          </div>

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {FEATURES.map((f) => (
              <FeatureCard key={f.title} {...f} />
            ))}
          </div>

          <div className="pt-6 pb-2 text-center text-[11px] text-muted-foreground/70">
            Want to see one of these in action? Hit{" "}
            <kbd className="rounded border border-border/70 bg-background px-1.5 py-0.5 font-mono text-[10px] text-foreground">
              ⌘?
            </kbd>{" "}
            to view the keyboard shortcuts.
          </div>
        </div>
      </div>
    </div>
  );
}
