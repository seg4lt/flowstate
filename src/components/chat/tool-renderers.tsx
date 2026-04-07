import * as React from "react";
import { structuredPatch, formatPatch, FILE_HEADERS_ONLY } from "diff";
import { PatchDiff } from "@pierre/diffs/react";
import { CodeBlock as ShikiCodeBlock } from "./messages/code-block";
import { MarkdownContent } from "./messages/markdown-content";
import { extractToolOutputText } from "@/lib/parse-tool-output";

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
    <div className="text-xs">
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
}: {
  oldStr: string;
  newStr: string;
  filePath?: string;
}) {
  const patchText = React.useMemo(
    () => buildUnifiedDiff(oldStr, newStr, filePath),
    [oldStr, newStr, filePath],
  );
  if (oldStr === newStr) return null;
  return (
    <div className="max-h-40 overflow-auto">
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

function EditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path);
  const oldStr = asString(a.old_string);
  const newStr = asString(a.new_string);
  const hasBoth = oldStr !== undefined && newStr !== undefined;
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
      {hasBoth ? (
        <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} />
      ) : (
        <>
          {oldStr !== undefined && (
            <div>
              <p className="mb-1 text-[11px] text-muted-foreground">old</p>
              <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px] text-destructive">
                {oldStr}
              </pre>
            </div>
          )}
          {newStr !== undefined && (
            <div>
              <p className="mb-1 text-[11px] text-muted-foreground">new</p>
              <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px] text-emerald-600 dark:text-emerald-400">
                {newStr}
              </pre>
            </div>
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
  const path = asString(a.file_path) ?? asString(a.path);
  const content = asString(a.content);
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
      {content !== undefined && (
        <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">
          {content}
        </pre>
      )}
      {!path && content === undefined && <GenericArgsRenderer args={args} />}
    </div>
  );
}

function MultiEditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path);
  const edits = Array.isArray(a.edits) ? a.edits : [];
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
            return (
              <div key={idx} className="space-y-1">
                <p className="text-muted-foreground">edit {idx + 1}</p>
                {hasBoth ? (
                  <DiffBlock oldStr={oldStr} newStr={newStr} filePath={path} />
                ) : (
                  <>
                    {oldStr !== undefined && (
                      <pre className="max-h-32 overflow-auto rounded bg-muted p-2 text-destructive">
                        {oldStr}
                      </pre>
                    )}
                    {newStr !== undefined && (
                      <pre className="max-h-32 overflow-auto rounded bg-muted p-2 text-emerald-600 dark:text-emerald-400">
                        {newStr}
                      </pre>
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

function ReadRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path);
  if (!path) return <GenericArgsRenderer args={args} />;
  const offset = a.offset;
  const limit = a.limit;
  return (
    <div className="space-y-1">
      <PathLine label="read" path={path} />
      {(offset !== undefined || limit !== undefined) && (
        <p className="text-[11px] text-muted-foreground">
          {offset !== undefined && <>offset {String(offset)} </>}
          {limit !== undefined && <>limit {String(limit)}</>}
        </p>
      )}
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

function TodoWriteRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const todos = Array.isArray(a.todos) ? a.todos : null;
  if (!todos) return <GenericArgsRenderer args={args} />;
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
  if (!isMarkdown) {
    return (
      <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded bg-muted/60 p-2 text-[11px] text-muted-foreground">
        {text}
      </pre>
    );
  }
  return (
    <div className="max-h-40 overflow-auto rounded bg-muted/60 p-2 text-xs text-muted-foreground [&_h1]:text-sm [&_h2]:text-xs [&_h3]:text-xs [&_pre]:text-[10px]">
      <MarkdownContent content={text} />
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

const RENDERERS: Record<string, RendererFn> = {
  ExitPlanMode: ExitPlanModeRenderer,
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
};

export function renderToolArgs(toolName: string, args: unknown): React.ReactElement {
  const Renderer = RENDERERS[toolName] ?? GenericArgsRenderer;
  return <Renderer args={args} />;
}

/** True if this tool's permission prompt should offer a post-approval mode picker. */
export function isPlanExitTool(toolName: string): boolean {
  return toolName === "ExitPlanMode";
}
