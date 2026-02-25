import * as React from "react";
import { MarkdownContent } from "./messages/markdown-content";

// Per-tool args renderers. Looked up by tool name. The default renders
// the args as pretty-printed JSON, which is what every tool used to do
// before — we override only for tools whose args contain a chunk of
// markdown, code, or a file path that deserves a friendlier surface.
//
// Adding a new renderer:
// 1. Write a small component that takes `args: any` and returns ReactNode.
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

function JsonFallback({ args }: RendererProps) {
  return (
    <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">
      {JSON.stringify(args, null, 2)}
    </pre>
  );
}

function ExitPlanModeRenderer({ args }: RendererProps) {
  const plan = asString(asRecord(args).plan);
  if (!plan) return <JsonFallback args={args} />;
  return (
    <div className="rounded-md border border-border bg-background p-3 text-sm leading-relaxed">
      <MarkdownContent content={plan} />
    </div>
  );
}

function CodeBlock({ language, code }: { language: string; code: string }) {
  return (
    <pre className="max-h-64 overflow-auto rounded bg-muted p-2 text-[11px]">
      <code className={`language-${language}`}>{code}</code>
    </pre>
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
  if (!command) return <JsonFallback args={args} />;
  return (
    <div className="space-y-1.5">
      {description && (
        <p className="text-xs text-muted-foreground">{description}</p>
      )}
      <CodeBlock language="bash" code={command} />
    </div>
  );
}

function EditRenderer({ args }: RendererProps) {
  const a = asRecord(args);
  const path = asString(a.file_path) ?? asString(a.path);
  const oldStr = asString(a.old_string);
  const newStr = asString(a.new_string);
  return (
    <div className="space-y-1.5">
      {path && <PathLine label="file" path={path} />}
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
      {!path && oldStr === undefined && newStr === undefined && (
        <JsonFallback args={args} />
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
      {!path && content === undefined && <JsonFallback args={args} />}
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
            return (
              <div key={idx} className="space-y-1">
                <p className="text-muted-foreground">edit {idx + 1}</p>
                {asString(e.old_string) !== undefined && (
                  <pre className="max-h-32 overflow-auto rounded bg-muted p-2 text-destructive">
                    {asString(e.old_string)}
                  </pre>
                )}
                {asString(e.new_string) !== undefined && (
                  <pre className="max-h-32 overflow-auto rounded bg-muted p-2 text-emerald-600 dark:text-emerald-400">
                    {asString(e.new_string)}
                  </pre>
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
  if (!path) return <JsonFallback args={args} />;
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
  if (!pattern) return <JsonFallback args={args} />;
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
  if (!pattern) return <JsonFallback args={args} />;
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
  if (!url) return <JsonFallback args={args} />;
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
  if (!query) return <JsonFallback args={args} />;
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
  if (!questions) return <JsonFallback args={args} />;
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
  if (!todos) return <JsonFallback args={args} />;
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
};

export function renderToolArgs(toolName: string, args: unknown): React.ReactElement {
  const Renderer = RENDERERS[toolName] ?? JsonFallback;
  return <Renderer args={args} />;
}

/** True if this tool's permission prompt should offer a post-approval mode picker. */
export function isPlanExitTool(toolName: string): boolean {
  return toolName === "ExitPlanMode";
}
