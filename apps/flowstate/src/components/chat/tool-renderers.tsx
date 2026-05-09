import * as React from "react";
import { structuredPatch, formatPatch, FILE_HEADERS_ONLY } from "diff";
import { PatchDiff } from "@pierre/diffs/react";
import { Maximize2, X } from "lucide-react";
import { CodeBlock as ShikiCodeBlock } from "./messages/code-block";
import { MarkdownContent } from "./messages/markdown-content";
import { Button } from "@/components/ui/button";
import { extractToolOutputText } from "@/lib/parse-tool-output";
import { languageFromPath } from "@/lib/language-from-path";

// Per-tool args renderers. Looked up by tool name. The default falls
// back to GenericArgsRenderer, which produces a labeled key/value view
// of any object — every tool gets a clean rendering for free, and the
// bespoke entries below only exist where key/value isn't enough (file
// diffs, markdown plans, sub-agent dispatches, etc.).
//
// Adding a new bespoke renderer:
// 1. Write a small component that takes `args: unknown` and returns ReactNode.
// 2. Register it in `RENDERERS` below under the exact tool name string.
// 3. Done — both ToolCallCard and PermissionPrompt pick it up.

interface RendererProps {
  args: unknown;
}

function asRecord(args: unknown): Record<string, unknown> {
  return args && typeof args === "object" ? (args as Record<string, unknown>) : {};
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

// Last-resort dump for values that can't be rendered structurally
// (deeply nested objects past one level, mixed-type arrays, etc.).
// Everything else should flow through GenericArgsRenderer.
function JsonFallback({ args }: RendererProps) {
  return (
    <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">
      {JSON.stringify(args, null, 2)}
    </pre>
  );
}

const SHORT_STRING_LIMIT = 60;

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.getPrototypeOf(value) === Object.prototype
  );
}

function FieldLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
      {children}
    </div>
  );
}

function renderScalar(value: unknown): React.ReactElement {
  if (value === null || value === undefined) {
    return <span className="font-mono text-muted-foreground">{value === null ? "null" : "—"}</span>;
  }
  if (typeof value === "boolean" || typeof value === "number") {
    return <span className="font-mono">{String(value)}</span>;
  }
  if (typeof value === "string") {
    if (value.length <= SHORT_STRING_LIMIT && !value.includes("\n")) {
      return <span className="font-mono break-words">{value}</span>;
    }
    return (
      <pre className="max-h-64 overflow-auto whitespace-pre-wrap rounded bg-muted p-2 text-[11px]">
        {value}
      </pre>
    );
  }
  return <JsonFallback args={value} />;
}

function renderValue(value: unknown, depth: number): React.ReactElement {
  if (Array.isArray(value)) {
    if (value.length === 0) {
      return <span className="text-muted-foreground">[]</span>;
    }
    if (depth >= 1) return <JsonFallback args={value} />;
    return (
      <ul className="space-y-1">
        {value.map((item, idx) => (
          <li key={idx} className="flex gap-2">
            <span className="text-muted-foreground">•</span>
            <div className="min-w-0 flex-1">{renderValue(item, depth + 1)}</div>
          </li>
        ))}
      </ul>
    );
  }
  if (isPlainObject(value)) {
    const entries = Object.entries(value);
    if (entries.length === 0) {
      return <span className="text-muted-foreground">{"{}"}</span>;
    }
    if (depth >= 1) return <JsonFallback args={value} />;
    return (
      <div className="space-y-2 border-l border-border/50 pl-3">
        {entries.map(([k, v]) => (
          <div key={k} className="space-y-0.5">
            <FieldLabel>{k}</FieldLabel>
            <div className="text-xs">{renderValue(v, depth + 1)}</div>
          </div>
        ))}
      </div>
    );
  }
  return renderScalar(value);
}

function GenericArgsRenderer({ args }: RendererProps) {
  if (!isPlainObject(args)) {
    return <div className="text-xs">{renderScalar(args)}</div>;
  }
  const entries = Object.entries(args);
  if (entries.length === 0) {
    return <p className="text-xs text-muted-foreground">no args</p>;
  }
  return (
    <div className="space-y-2">
      {entries.map(([k, v]) => (
        <div key={k} className="space-y-0.5">
          <FieldLabel>{k}</FieldLabel>
          <div className="text-xs">{renderValue(v, 0)}</div>
        </div>
      ))}
    </div>
  );
}

/** Plan markdown body without an outer frame — used by the permission
 *  banner so the markdown sits flush inside the amber container. The
 *  framed variant for the post-approval tool card lives in
 *  ExitPlanModeRenderer below. */
export function renderPlanBody(input: unknown): React.ReactElement | null {
  const plan = asString(asRecord(input).plan);
  if (!plan) return null;
  return (
    <div className="text-sm leading-relaxed">
      <MarkdownContent content={plan} />
    </div>
  );
}

function ExitPlanModeRenderer({ args }: RendererProps) {
  const plan = asString(asRecord(args).plan);
  if (!plan) return <GenericArgsRenderer args={args} />;
  return (
    <div className="rounded-md border border-border bg-background p-3 text-sm leading-relaxed">
      <MarkdownContent content={plan} />
    </div>
  );
}

// Wrapper that leans on the shared shiki-powered CodeBlock used by
// MarkdownContent — gives Bash and friends real syntax highlighting
// instead of a plain monospace blob. The shared component applies its
// own mb-3 which collides with the tight tool-card layout, so we wrap
// it in a zero-margin container and let the last:mb-0 rule take over.
function CodeBlock({ language, code }: { language: string; code: string }) {
  return (
    <div className="[&>*]:!mb-0 [&_pre]:!whitespace-pre-wrap [&_pre]:!break-words">
      <ShikiCodeBlock code={code} language={language} />
    </div>
  );
}

function PathLine({ label, path }: { label?: string; path: string }) {
  return (
    <div className="min-w-0 break-all text-xs">
      {label && <span className="text-muted-foreground">{label} </span>}
      <span className="font-mono">{path}</span>
    </div>
  );
}

function BashRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const command = asString(a.command);
  const description = asString(a.description);
  if (!command) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1.5">
      {description && (
        <p className="text-xs text-muted-foreground">{description}</p>
      )}
      <CodeBlock language="bash" code={command} />
    </div>
  );
}

/**
 * Compute a compact unified diff string from old/new text snippets.
 * Returns only hunk headers + content lines (no file headers),
 * suitable for rendering with Shiki's "diff" language grammar.
 */
function buildUnifiedDiff(oldStr: string, newStr: string, filePath?: string): string {
  const a = oldStr.endsWith("\n") ? oldStr : oldStr + "\n";
  const b = newStr.endsWith("\n") ? newStr : newStr + "\n";
  const name = filePath ?? "file";
  const patch = structuredPatch(name, name, a, b, undefined, undefined, {
    context: 3,
  });
  return formatPatch(patch, FILE_HEADERS_ONLY).replace(/\n$/, "");
}

function DiffBlock({
  oldStr,
  newStr,
  filePath,
  expanded = false,
}: {
  oldStr: string;
  newStr: string;
  filePath?: string;
  /**
   * `true` removes the inline height cap so the diff renders at full
   * size inside the zoom popup. The popup provides its own outer
   * `max-h-[90vh]` scroll container, so we don't want a second cap
   * forcing nested scrollbars.
   */
  expanded?: boolean;
}) {
  const patchText = React.useMemo(
    () => buildUnifiedDiff(oldStr, newStr, filePath),
    [oldStr, newStr, filePath],
  );
  if (oldStr === newStr) return null;
  // Heights here (and in the Edit/Write/MultiEdit renderers below)
  // were nudged ~20% above the original tight values: small content
  // still shrinks to fit, larger content gets a bit more breathing
  // room before the scroll kicks in. Hard cap intentional — these
  // cards live inside a chat transcript and must never grow to
  // fill the viewport. The zoom popup opts out by passing
  // `expanded`, deferring scroll to the popup's outer container.
  return (
    <div className={expanded ? "" : "max-h-48 overflow-auto"}>
      <PatchDiff
        patch={patchText}
        options={{
          diffStyle: "unified",
          theme: { dark: "pierre-dark", light: "pierre-light" },
          themeType: "system",
          diffIndicators: "classic",
          overflow: "scroll",
          disableFileHeader: true,
          maxLineDiffLength: 2_000,
          tokenizeMaxLineLength: 5_000,
        }}
      />
    </div>
  );
}

/**
 * Wraps an inline tool-result block (a diff, a code preview, an old/new
 * `<pre>` pair, …) with a small Maximize2 button in the top-right that
 * opens a popup with a larger view of the same content.
 *
 * Inline rendering is **unchanged** — the wrapper only adds the button
 * and (when open) renders a sibling overlay. The popup is intentionally
 * low-key: no backdrop dim, no pointer-event capture on empty space, ESC
 * to close, click the X to close. Mirrors the existing
 * `ExpandedOutputOverlay` pattern used by `ToolOutputContent` so it
 * looks/feels like the rest of the UI.
 *
 * `expanded` lets callers pass a richer rendering for the popup (e.g. a
 * `DiffBlock` without the `max-h-48` cap) while keeping the original
 * cramped version inline. Falls back to `children` when omitted.
 */
function ZoomablePanel({
  title,
  children,
  expanded,
}: {
  title?: string;
  children: React.ReactNode;
  expanded?: React.ReactNode;
}) {
  const [open, setOpen] = React.useState(false);

  // ESC closes the popup. We register only while open so we don't
  // compete with other ESC handlers (menus, modals) when collapsed.
  React.useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.stopPropagation();
        setOpen(false);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  return (
    <>
      <div className="relative">
        <Button
          variant="ghost"
          size="icon-xs"
          onClick={() => setOpen(true)}
          aria-label="Open larger view"
          title="Zoom"
          className="absolute top-1 right-1 z-10 bg-background/80 text-muted-foreground hover:text-foreground"
        >
          <Maximize2 className="h-3 w-3" />
        </Button>
        {children}
      </div>
      {open && (
        <ZoomOverlay title={title} onClose={() => setOpen(false)}>
          {expanded ?? children}
        </ZoomOverlay>
      )}
    </>
  );
}

function ZoomOverlay({
  title,
  children,
  onClose,
}: {
  title?: string;
  children: React.ReactNode;
  onClose: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="false"
      aria-label={title ?? "Zoomed view"}
      className="pointer-events-none fixed inset-0 z-50 flex items-center justify-center p-6"
    >
      <div className="pointer-events-auto flex max-h-[90vh] w-full max-w-5xl flex-col overflow-hidden rounded-lg border border-border bg-popover text-popover-foreground shadow-xl ring-1 ring-foreground/10">
        <div className="flex items-center justify-between gap-2 border-b border-border px-3 py-2">
          <span className="truncate text-xs font-medium text-muted-foreground">
            {title ?? "Zoomed view"}
          </span>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onClose}
            aria-label="Close"
            title="Close"
          >
            <X className="h-3 w-3" />
          </Button>
        </div>
        <div className="overflow-auto p-3">{children}</div>
      </div>
    </div>
  );
}

function EditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  // Field-name compatibility across providers:
  //  - Claude / opencode: file_path + old_string + new_string
  //  - Copilot built-in `edit`: path + old_str + new_str
  //  - Some opencode flavours emit camelCase: filePath / oldString / newString
  // We accept all spellings so a single renderer covers every provider.
  const path = asString(a.file_path) ?? asString(a.path) ?? asString(a.filePath);
  const oldStr =
    asString(a.old_string) ?? asString(a.old_str) ?? asString(a.oldString);
  const newStr =
    asString(a.new_string) ?? asString(a.new_str) ?? asString(a.newString);
  const hasBoth = oldStr !== undefined && newStr !== undefined;
  const zoomTitle = path ?? "edit";
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
      {hasBoth ? (
        <ZoomablePanel
          title={zoomTitle}
          expanded={
            <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} expanded />
          }
        >
          <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} />
        </ZoomablePanel>
      ) : (
        <>
          {oldStr !== undefined && (
            <ZoomablePanel
              title={`${zoomTitle} (old)`}
              expanded={
                <pre className="rounded bg-muted p-2 text-xs text-destructive">
                  {oldStr}
                </pre>
              }
            >
              <div>
                <p className="mb-1 text-[11px] text-muted-foreground">old</p>
                <pre className="max-h-48 overflow-auto rounded bg-muted p-2 text-[11px] text-destructive">
                  {oldStr}
                </pre>
              </div>
            </ZoomablePanel>
          )}
          {newStr !== undefined && (
            <ZoomablePanel
              title={`${zoomTitle} (new)`}
              expanded={
                <pre className="rounded bg-muted p-2 text-xs text-emerald-600 dark:text-emerald-400">
                  {newStr}
                </pre>
              }
            >
              <div>
                <p className="mb-1 text-[11px] text-muted-foreground">new</p>
                <pre className="max-h-48 overflow-auto rounded bg-muted p-2 text-[11px] text-emerald-600 dark:text-emerald-400">
                  {newStr}
                </pre>
              </div>
            </ZoomablePanel>
          )}
        </>
      )}
      {!path && oldStr === undefined && newStr === undefined && (
        <GenericArgsRenderer args={args} />
      )}
    </div>
  );
}

function WriteRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  // Same multi-provider field tolerance as EditRenderer. Copilot's `create`
  // tool uses `file_text` for the body; opencode/Claude use `content`.
  const path = asString(a.file_path) ?? asString(a.path) ?? asString(a.filePath);
  const content =
    asString(a.content) ?? asString(a.file_text) ?? asString(a.fileText);
  // Infer language from the file extension so the preview gets the
  // same shiki highlighting as the file viewer / Bash tool card,
  // rather than a flat monospace blob. Falls back to "text" when
  // there's no path or no extension.
  const language = path ? languageFromPath(path) : "text";
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
      {content !== undefined && (
        <ZoomablePanel
          title={path ?? "write"}
          expanded={<CodeBlock language={language} code={content} />}
        >
          <div className="max-h-[19.2rem] overflow-auto">
            <CodeBlock language={language} code={content} />
          </div>
        </ZoomablePanel>
      )}
      {!path && content === undefined && <GenericArgsRenderer args={args} />}
    </div>
  );
}

function MultiEditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path);
  const edits = Array.isArray(a.edits) ? a.edits : [];
  const zoomBase = path ?? "multi-edit";
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
      <p className="text-xs text-muted-foreground">{edits.length} edit{edits.length === 1 ? "" : "s"}</p>
      <details className="text-[11px]">
        <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
          show all edits
        </summary>
        <div className="mt-2 space-y-2">
          {edits.map((edit, idx) => {
            const e = asRecord(edit);
            const oldStr = asString(e.old_string);
            const newStr = asString(e.new_string);
            const hasBoth = oldStr !== undefined && newStr !== undefined;
            const editTitle = `${zoomBase} — edit ${idx + 1}`;
            return (
              <div key={idx} className="space-y-1">
                <p className="text-muted-foreground">edit {idx + 1}</p>
                {hasBoth ? (
                  <ZoomablePanel
                    title={editTitle}
                    expanded={
                      <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} expanded />
                    }
                  >
                    <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} />
                  </ZoomablePanel>
                ) : (
                  <>
                    {oldStr !== undefined && (
                      <ZoomablePanel
                        title={`${editTitle} (old)`}
                        expanded={
                          <pre className="rounded bg-muted p-2 text-xs text-destructive">
                            {oldStr}
                          </pre>
                        }
                      >
                        <pre className="max-h-[9.6rem] overflow-auto rounded bg-muted p-2 text-destructive">
                          {oldStr}
                        </pre>
                      </ZoomablePanel>
                    )}
                    {newStr !== undefined && (
                      <ZoomablePanel
                        title={`${editTitle} (new)`}
                        expanded={
                          <pre className="rounded bg-muted p-2 text-xs text-emerald-600 dark:text-emerald-400">
                            {newStr}
                          </pre>
                        }
                      >
                        <pre className="max-h-[9.6rem] overflow-auto rounded bg-muted p-2 text-emerald-600 dark:text-emerald-400">
                          {newStr}
                        </pre>
                      </ZoomablePanel>
                    )}
                  </>
                )}
              </div>
            );
          })}
        </div>
      </details>
    </div>
  );
}

// ---------------------------------------------------------------------------
// apply_patch (Copilot / Codex)
//
// Both providers use OpenAI's V4A patch format: a single freeform string
// passed as `args.input` (Copilot's custom-tool wrapper stores the raw
// patch under `input`; Codex emits the same shape). We parse it into
// per-file ops and reuse the existing DiffBlock for Update hunks so the
// diff highlighting matches what Edit produces.
//
// V4A grammar (from the Copilot CLI's lark definition):
//   *** Begin Patch
//   <op> ...
//   *** End Patch
// where <op> is one of:
//   *** Update File: <path>            (optionally followed by `*** Move to: <new path>`)
//     @@ <optional context>            (zero or more @@ headers per file)
//      / + / - prefixed change lines
//     *** End of File                  (optional sentinel)
//   *** Add File: <path>
//     + prefixed lines (full content)
//   *** Delete File: <path>
// ---------------------------------------------------------------------------
type ApplyPatchHunk = { header?: string; oldStr: string; newStr: string };
type ApplyPatchOp =
  | { kind: "update"; path: string; moveTo?: string; hunks: ApplyPatchHunk[] }
  | { kind: "add"; path: string; content: string }
  | { kind: "delete"; path: string };

function parseV4APatch(text: string): ApplyPatchOp[] | null {
  const lines = text.split("\n");
  // Skip optional surrounding whitespace and find Begin/End Patch markers.
  let i = 0;
  while (i < lines.length && lines[i].trim() === "") i++;
  if (i >= lines.length || lines[i].trim() !== "*** Begin Patch") return null;
  i++;

  const ops: ApplyPatchOp[] = [];
  let current: ApplyPatchOp | null = null;
  // For Update ops we accumulate a single in-flight hunk's old/new buffers
  // until we hit a new @@ header or a new top-level op.
  let hunkHeader: string | undefined;
  let hunkOld = "";
  let hunkNew = "";
  let hunkActive = false;

  function flushHunk() {
    if (!hunkActive || !current || current.kind !== "update") return;
    // Strip the trailing newline we appended on the last change line so the
    // diff renderer doesn't see an artificial empty line at the end.
    const o = hunkOld.endsWith("\n") ? hunkOld.slice(0, -1) : hunkOld;
    const n = hunkNew.endsWith("\n") ? hunkNew.slice(0, -1) : hunkNew;
    current.hunks.push({ header: hunkHeader, oldStr: o, newStr: n });
    hunkHeader = undefined;
    hunkOld = "";
    hunkNew = "";
    hunkActive = false;
  }

  function flushOp() {
    flushHunk();
    if (current) ops.push(current);
    current = null;
  }

  for (; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === "*** End Patch") {
      flushOp();
      return ops;
    }
    if (line.startsWith("*** Update File: ")) {
      flushOp();
      current = { kind: "update", path: line.slice("*** Update File: ".length).trim(), hunks: [] };
      continue;
    }
    if (line.startsWith("*** Add File: ")) {
      flushOp();
      current = { kind: "add", path: line.slice("*** Add File: ".length).trim(), content: "" };
      continue;
    }
    if (line.startsWith("*** Delete File: ")) {
      flushOp();
      current = { kind: "delete", path: line.slice("*** Delete File: ".length).trim() };
      continue;
    }
    if (line.startsWith("*** Move to: ") && current?.kind === "update") {
      current.moveTo = line.slice("*** Move to: ".length).trim();
      continue;
    }
    if (line === "*** End of File") {
      // Sentinel — just close the active hunk; a new @@ or *** op will follow.
      flushHunk();
      continue;
    }
    if (line.startsWith("@@") && current?.kind === "update") {
      flushHunk();
      hunkHeader = line.slice(2).trim() || undefined;
      hunkActive = true;
      continue;
    }
    if (!current) continue;
    if (current.kind === "add") {
      // Add File contents are `+` prefixed; strip the prefix to recover the
      // raw file body. Lines without a prefix are tolerated as-is so a
      // mildly malformed patch still previews readably.
      const stripped = line.startsWith("+") ? line.slice(1) : line;
      current.content += (current.content ? "\n" : "") + stripped;
      continue;
    }
    if (current.kind === "update") {
      if (!hunkActive) {
        // First change line without an explicit @@ header — open an
        // anonymous hunk so the body still renders.
        hunkActive = true;
      }
      const ch = line.charAt(0);
      const rest = line.slice(1);
      if (ch === " ") {
        hunkOld += rest + "\n";
        hunkNew += rest + "\n";
      } else if (ch === "-") {
        hunkOld += rest + "\n";
      } else if (ch === "+") {
        hunkNew += rest + "\n";
      } else if (line === "") {
        // Blank line inside a hunk — treat as an unchanged empty line.
        hunkOld += "\n";
        hunkNew += "\n";
      }
      // Anything else (stray text) is ignored; the renderer falls back to
      // raw display if no recognised hunks accumulated.
      continue;
    }
  }
  // No End-Patch marker — accept what we parsed so partial / streaming
  // payloads still preview rather than collapsing to the JSON fallback.
  flushOp();
  return ops.length > 0 ? ops : null;
}

function ApplyPatchRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  // Copilot's custom-tool bridge wraps freeform input as `{ input: <patch> }`;
  // Codex uses the same shape. Fall back to a `patch` field for tolerance.
  const raw = asString(a.input) ?? asString(a.patch);
  if (!raw) return <GenericArgsRenderer args={args} />;
  const ops = parseV4APatch(raw);
  if (!ops || ops.length === 0) {
    // Couldn't parse — show the raw patch text in a zoomable panel so the
    // user still gets something readable instead of a JSON dump.
    return (
      <ZoomablePanel
        title="apply_patch"
        expanded={
          <pre className="rounded bg-muted p-2 text-xs whitespace-pre-wrap">{raw}</pre>
        }
      >
        <pre className="max-h-48 overflow-auto rounded bg-muted p-2 text-[11px] whitespace-pre-wrap">
          {raw}
        </pre>
      </ZoomablePanel>
    );
  }
  return (
    <div className="space-y-3">
      {ops.map((op, idx) => {
        if (op.kind === "delete") {
          return (
            <div key={idx} className="space-y-1">
              <PathLine label="delete" path={op.path} />
            </div>
          );
        }
        if (op.kind === "add") {
          const language = languageFromPath(op.path);
          return (
            <div key={idx} className="space-y-1.5">
              <PathLine label="add" path={op.path} />
              {op.content && (
                <ZoomablePanel
                  title={op.path}
                  expanded={<CodeBlock language={language} code={op.content} />}
                >
                  <div className="max-h-[19.2rem] overflow-auto">
                    <CodeBlock language={language} code={op.content} />
                  </div>
                </ZoomablePanel>
              )}
            </div>
          );
        }
        // update
        return (
          <div key={idx} className="space-y-1.5">
            <PathLine label="edit" path={op.path} />
            {op.moveTo && <PathLine label="move to" path={op.moveTo} />}
            {op.hunks.length === 0 ? (
              <p className="text-[11px] text-muted-foreground">no hunks</p>
            ) : (
              op.hunks.map((h, hi) => {
                const title = `${op.path}${op.hunks.length > 1 ? ` — hunk ${hi + 1}` : ""}`;
                return (
                  <div key={hi} className="space-y-1">
                    {h.header && (
                      <p className="font-mono text-[10px] text-muted-foreground">
                        @@ {h.header}
                      </p>
                    )}
                    <ZoomablePanel
                      title={title}
                      expanded={
                        <DiffBlock
                          oldStr={h.oldStr}
                          newStr={h.newStr}
                          filePath={op.path}
                          expanded
                        />
                      }
                    >
                      <DiffBlock oldStr={h.oldStr} newStr={h.newStr} filePath={op.path} />
                    </ZoomablePanel>
                  </div>
                );
              })
            )}
          </div>
        );
      })}
    </div>
  );
}

function ReadRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path) ?? asString(a.filePath);
  if (!path) return <GenericArgsRenderer args={args} />;
  const offset = a.offset;
  const limit = a.limit;
  // Copilot's `view` reuses this renderer and surfaces a `[start, end]`
  // pair instead of offset/limit. Show the same one-liner.
  const viewRange = Array.isArray(a.view_range) ? a.view_range : null;
  return (
    <div className="space-y-1">
      <PathLine label="read" path={path} />
      {(offset !== undefined || limit !== undefined) && (
        <p className="text-[11px] text-muted-foreground">
          {offset !== undefined && <>offset {String(offset)} </>}
          {limit !== undefined && <>limit {String(limit)}</>}
        </p>
      )}
      {viewRange && viewRange.length === 2 && (
        <p className="text-[11px] text-muted-foreground">
          lines {String(viewRange[0])}–{String(viewRange[1])}
        </p>
      )}
    </div>
  );
}

// Copilot's `insert` tool: insert `new_str` at `insert_line` of `path`.
// Renders the inserted snippet with file-extension syntax highlighting,
// same shape as Write's preview but labeled with the line number.
function InsertRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.path) ?? asString(a.file_path);
  const insertLine = a.insert_line ?? a.insertLine;
  const newStr = asString(a.new_str) ?? asString(a.new_string) ?? asString(a.newString);
  if (!path && newStr === undefined) return <GenericArgsRenderer args={args} />;
  const language = path ? languageFromPath(path) : "text";
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="insert" path={path} />}
      {insertLine !== undefined && (
        <p className="text-[11px] text-muted-foreground">
          at line {String(insertLine)}
        </p>
      )}
      {newStr !== undefined && (
        <ZoomablePanel
          title={path ?? "insert"}
          expanded={<CodeBlock language={language} code={newStr} />}
        >
          <div className="max-h-[19.2rem] overflow-auto">
            <CodeBlock language={language} code={newStr} />
          </div>
        </ZoomablePanel>
      )}
    </div>
  );
}

// Codex `fileChange` tool card: the item carries a `changes` array where
// each entry is `{ path, kind: { type: "add" | "delete" | "update" }, diff }`.
// The same data also fans out to dedicated FileChange events the UI renders
// elsewhere; this card just lists the affected files so the tool entry
// isn't a JSON dump.
function CodexFileChangeRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const changes = Array.isArray(a.changes) ? a.changes : null;
  if (!changes || changes.length === 0) {
    return <GenericArgsRenderer args={args} />;
  }
  return (
    <div className="space-y-1">
      {changes.map((change, idx) => {
        const c = asRecord(change);
        const path = asString(c.path);
        const kindObj = c.kind;
        const kind = isPlainObject(kindObj)
          ? asString(kindObj.type)
          : asString(kindObj);
        const label = kind === "add" ? "add" : kind === "delete" ? "delete" : "edit";
        if (!path) return null;
        return <PathLine key={idx} label={label} path={path} />;
      })}
    </div>
  );
}

function GlobRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const pattern = asString(a.pattern);
  const path = asString(a.path);
  if (!pattern) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1 text-xs">
      <div>
        <span className="text-muted-foreground">pattern </span>
        <span className="font-mono">{pattern}</span>
      </div>
      {path && <PathLine label="in" path={path} />}
    </div>
  );
}

function GrepRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const pattern = asString(a.pattern);
  const path = asString(a.path);
  const glob = asString(a.glob);
  if (!pattern) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1 text-xs">
      <div>
        <span className="text-muted-foreground">grep </span>
        <span className="font-mono">{pattern}</span>
      </div>
      {path && <PathLine label="in" path={path} />}
      {glob && <PathLine label="glob" path={glob} />}
    </div>
  );
}

function WebFetchRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const url = asString(a.url);
  const prompt = asString(a.prompt);
  if (!url) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1 text-xs">
      <PathLine label="fetch" path={url} />
      {prompt && <p className="text-muted-foreground">{prompt}</p>}
    </div>
  );
}

function WebSearchRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const query = asString(a.query);
  if (!query) return <GenericArgsRenderer args={args} />;
  return (
    <div className="text-xs">
      <span className="text-muted-foreground">search </span>
      <span className="font-mono">{query}</span>
    </div>
  );
}

function AskUserQuestionRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const questions = Array.isArray(a.questions) ? a.questions : null;
  if (!questions) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-3">
      {questions.map((q, idx) => {
        const question = asRecord(q);
        const text = asString(question.question) ?? asString(question.text) ?? "";
        const header = asString(question.header);
        const options = Array.isArray(question.options) ? question.options : [];
        return (
          <div key={idx} className="space-y-1.5">
            {header && (
              <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                {header}
              </div>
            )}
            <div className="text-xs">{text}</div>
            <ul className="space-y-0.5 text-[11px]">
              {options.map((opt, oi) => {
                const o = asRecord(opt);
                return (
                  <li key={oi} className="flex gap-2">
                    <span className="text-muted-foreground">•</span>
                    <div>
                      <span className="font-medium">{asString(o.label)}</span>
                      {asString(o.description) && (
                        <span className="text-muted-foreground"> — {asString(o.description)}</span>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        );
      })}
    </div>
  );
}

export function TodoList({ todos }: { todos: unknown[] }) {
  return (
    <ul className="space-y-1 text-xs">
      {todos.map((todo, idx) => {
        const t = asRecord(todo);
        const content = asString(t.content) ?? asString(t.subject) ?? "";
        const status = asString(t.status) ?? "pending";
        const marker =
          status === "completed" ? "✓" : status === "in_progress" ? "▶" : "○";
        return (
          <li key={idx} className="flex gap-2">
            <span className="text-muted-foreground">{marker}</span>
            <span className={status === "completed" ? "line-through text-muted-foreground" : ""}>
              {content}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

function TodoWriteRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const todos = Array.isArray(a.todos) ? a.todos : null;
  if (!todos) return <GenericArgsRenderer args={args} />;
  return <TodoList todos={todos} />;
}

function CollapsibleMarkdown({ label, body }: { label: string; body: string }) {
  const collapse = body.length > 400;
  if (!collapse) {
    return (
      <div className="space-y-1">
        <FieldLabel>{label}</FieldLabel>
        <div className="text-sm leading-relaxed">
          <MarkdownContent content={body} />
        </div>
      </div>
    );
  }
  return (
    <details className="space-y-1">
      <summary className="cursor-pointer select-none text-[10px] font-medium uppercase tracking-wide text-muted-foreground hover:text-foreground">
        {label}
      </summary>
      <div className="mt-1 text-sm leading-relaxed">
        <MarkdownContent content={body} />
      </div>
    </details>
  );
}

// ---------------------------------------------------------------------------
// Sub-agent / Task output: parse the JSON content-block array and render
// the extracted text as markdown inside a compact scrollable container.
// Falls back to a plain <pre> when the output isn't the expected format.
// ---------------------------------------------------------------------------
export function ToolOutputContent({ output }: { output: string }) {
  const { text, isMarkdown } = extractToolOutputText(output);
  const [expanded, setExpanded] = React.useState(false);

  // ESC closes the expanded overlay. We register the listener only while
  // the overlay is open so it doesn't compete with other ESC handlers
  // (e.g. closing menus) on the page when collapsed.
  React.useEffect(() => {
    if (!expanded) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.stopPropagation();
        setExpanded(false);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [expanded]);

  if (!isMarkdown) {
    return (
      <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded bg-muted/60 p-2 text-[11px] text-muted-foreground">
        {text}
      </pre>
    );
  }

  return (
    <>
      <div className="relative">
        <Button
          variant="ghost"
          size="icon-xs"
          onClick={() => setExpanded(true)}
          aria-label="Expand output"
          title="Expand"
          className="absolute top-1 right-1 z-10 bg-background/80 text-muted-foreground hover:text-foreground"
        >
          <Maximize2 className="h-3 w-3" />
        </Button>
        <div className="max-h-40 overflow-auto rounded bg-muted/60 p-2 text-xs text-muted-foreground [&_h1]:text-sm [&_h2]:text-xs [&_h3]:text-xs [&_pre]:text-[10px]">
          <MarkdownContent content={text} />
        </div>
      </div>
      {expanded && <ExpandedOutputOverlay text={text} onClose={() => setExpanded(false)} />}
    </>
  );
}

// Low-key floating panel — opaque, no backdrop dim/blur. Sits above the
// page on a high z-index, but doesn't block the rest of the UI visually.
// Close via ESC (handled in ToolOutputContent) or the X button.
function ExpandedOutputOverlay({
  text,
  onClose,
}: {
  text: string;
  onClose: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="false"
      aria-label="Expanded tool output"
      className="pointer-events-none fixed inset-0 z-50 flex items-center justify-center p-6"
    >
      <div className="pointer-events-auto flex max-h-[85vh] w-full max-w-3xl flex-col overflow-hidden rounded-lg border border-border bg-popover text-popover-foreground shadow-xl ring-1 ring-foreground/10">
        <div className="flex items-center justify-between gap-2 border-b border-border px-3 py-2">
          <span className="text-xs font-medium text-muted-foreground">
            Output
          </span>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onClose}
            aria-label="Close"
            title="Close"
          >
            <X className="h-3 w-3" />
          </Button>
        </div>
        <div className="overflow-auto p-4 text-sm [&_pre]:text-xs">
          <MarkdownContent content={text} />
        </div>
      </div>
    </div>
  );
}

function TaskRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const description = asString(a.description);
  const subagentType = asString(a.subagent_type);
  const prompt = asString(a.prompt);
  if (!description && !subagentType && !prompt) {
    return <GenericArgsRenderer args={args} />;
  }
  return (
    <div className="space-y-2">
      {(description || subagentType) && (
        <div className="flex flex-wrap items-center gap-2">
          {description && (
            <span className="text-xs font-medium">{description}</span>
          )}
          {subagentType && (
            <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
              {subagentType}
            </span>
          )}
        </div>
      )}
      {prompt && <CollapsibleMarkdown label="prompt" body={prompt} />}
    </div>
  );
}

function SkillRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const skill = asString(a.skill);
  const skillArgs = asString(a.args);
  if (!skill) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-2 text-xs">
        <span className="text-muted-foreground">skill</span>
        <span className="rounded bg-muted px-1.5 py-0.5 font-mono">{skill}</span>
      </div>
      {skillArgs && <CodeBlock language="text" code={skillArgs} />}
    </div>
  );
}

function NotebookEditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.notebook_path) ?? asString(a.file_path);
  const cellId = asString(a.cell_id);
  const cellType = asString(a.cell_type);
  const editMode = asString(a.edit_mode);
  const newSource = asString(a.new_source);
  if (!path && !newSource) return <GenericArgsRenderer args={args} />;
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="notebook" path={path} />}
      {(cellId || cellType || editMode) && (
        <div className="flex flex-wrap gap-2 text-[11px] text-muted-foreground">
          {cellId && <span>cell {cellId}</span>}
          {cellType && <span>type {cellType}</span>}
          {editMode && <span>mode {editMode}</span>}
        </div>
      )}
      {newSource && (
        <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">
          {newSource}
        </pre>
      )}
    </div>
  );
}

function ScheduleWakeupRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const delay = a.delaySeconds;
  const reason = asString(a.reason);
  const prompt = asString(a.prompt);
  if (delay === undefined && !reason && !prompt) {
    return <GenericArgsRenderer args={args} />;
  }
  return (
    <div className="space-y-1.5 text-xs">
      <div className="flex flex-wrap gap-3">
        {delay !== undefined && (
          <div>
            <span className="text-muted-foreground">in </span>
            <span className="font-mono">{String(delay)}s</span>
          </div>
        )}
        {reason && (
          <div className="min-w-0 flex-1">
            <span className="text-muted-foreground">reason </span>
            <span>{reason}</span>
          </div>
        )}
      </div>
      {prompt && (
        <details>
          <summary className="cursor-pointer select-none text-[10px] font-medium uppercase tracking-wide text-muted-foreground hover:text-foreground">
            wake prompt
          </summary>
          <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap rounded bg-muted p-2 text-[11px]">
            {prompt}
          </pre>
        </details>
      )}
    </div>
  );
}

type RendererFn = (props: RendererProps) => React.ReactElement;

function EnterPlanModeRenderer({ args: _args }: RendererProps) {
  return (
    <div className="text-sm text-muted-foreground">Switched to plan mode</div>
  );
}

const RENDERERS: Record<string, RendererFn> = {
  ExitPlanMode: ExitPlanModeRenderer,
  EnterPlanMode: EnterPlanModeRenderer,
  Bash: BashRenderer,
  Edit: EditRenderer,
  Write: WriteRenderer,
  MultiEdit: MultiEditRenderer,
  Read: ReadRenderer,
  Glob: GlobRenderer,
  Grep: GrepRenderer,
  WebFetch: WebFetchRenderer,
  WebSearch: WebSearchRenderer,
  TodoWrite: TodoWriteRenderer,
  AskUserQuestion: AskUserQuestionRenderer,
  Task: TaskRenderer,
  Agent: TaskRenderer,
  Skill: SkillRenderer,
  NotebookEdit: NotebookEditRenderer,
  ScheduleWakeup: ScheduleWakeupRenderer,
  // Copilot (and Codex) use a single freeform V4A patch tool. Render it
  // with the same diff highlighting Edit/MultiEdit get.
  apply_patch: ApplyPatchRenderer,

  // ---------------------------------------------------------------------
  // Cross-provider aliases. Each entry maps a non-Claude tool name to one
  // of the renderers above so the UI shows a real preview instead of a
  // generic key/value table. The renderers themselves accept multiple
  // arg-field spellings (file_path / path / filePath, old_string /
  // old_str / oldString, content / file_text / fileText), so the same
  // function works for every provider that ships a tool with that
  // semantic.
  //
  // OpenCode (lowercase, Claude-compatible args):
  bash: BashRenderer,
  read: ReadRenderer,
  write: WriteRenderer,
  edit: EditRenderer,
  glob: GlobRenderer,
  grep: GrepRenderer,
  webfetch: WebFetchRenderer,
  websearch: WebSearchRenderer,
  task: TaskRenderer,
  todowrite: TodoWriteRenderer,
  skill: SkillRenderer,
  //
  // GitHub Copilot built-in tools (per @github/copilot/app.js):
  //   `view`   → Read   (path, view_range?)
  //   `create` → Write  (path, file_text)
  //   `edit`   → Edit   (path, old_str, new_str) — alias above already
  //                     covers the name; renderer accepts old_str/new_str
  //   `insert` → InsertRenderer (path, insert_line, new_str)
  view: ReadRenderer,
  create: WriteRenderer,
  insert: InsertRenderer,
  //
  // Codex display names (provider-codex/src/lib.rs `tool_item_display_name`
  // emits these literal strings — note the spaces). The actual file diffs
  // also fan out as separate FileChange events; this card just lists the
  // affected paths so it isn't a noisy JSON dump.
  "File change": CodexFileChangeRenderer,
  "Web search": WebSearchRenderer,
};

export function renderToolArgs(toolName: string, args: unknown): React.ReactElement {
  const Renderer = RENDERERS[toolName] ?? GenericArgsRenderer;
  return <Renderer args={args} />;
}

/** True if this tool's permission prompt should offer a post-approval mode picker. */
export function isPlanExitTool(toolName: string): boolean {
  return toolName === "ExitPlanMode";
}

/** True if this tool signals the agent wants to enter plan mode. */
export function isPlanEnterTool(toolName: string): boolean {
  return toolName === "EnterPlanMode";
}
