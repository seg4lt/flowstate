import { useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  FilePenLine,
  FilePlus,
  FileX,
  Terminal,
  Wrench,
} from "lucide-react";
import type {
  FileChangeRecord,
  SubagentRecord,
  ToolCall,
} from "../../types";

interface Props {
  toolCalls: ToolCall[];
  fileChanges: FileChangeRecord[];
  subagents: SubagentRecord[];
  turnCompleted: boolean;
}

type WorkEntry =
  | { kind: "tool"; call: ToolCall; fileChange?: FileChangeRecord; subagent?: SubagentRecord }
  | { kind: "file"; fileChange: FileChangeRecord };

function buildEntries(props: Props): WorkEntry[] {
  const entries: WorkEntry[] = [];
  const consumedFileCalls = new Set<string>();
  const consumedSubagents = new Set<string>();

  for (const call of props.toolCalls) {
    const fc = props.fileChanges.find((x) => x.callId === call.callId);
    if (fc) consumedFileCalls.add(fc.callId);
    const sub = props.subagents.find((x) => x.parentCallId === call.callId);
    if (sub) consumedSubagents.add(sub.agentId);
    entries.push({ kind: "tool", call, fileChange: fc, subagent: sub });
  }

  for (const fc of props.fileChanges) {
    if (consumedFileCalls.has(fc.callId)) continue;
    entries.push({ kind: "file", fileChange: fc });
  }

  return entries;
}

function describeToolCall(call: ToolCall, fileChange?: FileChangeRecord): {
  label: string;
  detail: string;
  Icon: typeof Wrench;
} {
  const name = call.name.toLowerCase();
  if (name === "bash" || name === "shell") {
    const command =
      ((call.args as { command?: string })?.command ?? "").split("\n")[0] ?? call.name;
    return { label: "Ran command", detail: command, Icon: Terminal };
  }
  if (fileChange) {
    const verb =
      fileChange.operation === "write"
        ? "Wrote"
        : fileChange.operation === "delete"
          ? "Deleted"
          : "Edited";
    const Icon =
      fileChange.operation === "write"
        ? FilePlus
        : fileChange.operation === "delete"
          ? FileX
          : FilePenLine;
    return { label: `${verb}`, detail: fileChange.path, Icon };
  }
  const argSummary =
    typeof call.args === "object" && call.args !== null
      ? Object.keys(call.args as Record<string, unknown>).slice(0, 2).join(", ")
      : "";
  return { label: call.name, detail: argSummary, Icon: Wrench };
}

function ToolCallRow({ entry }: { entry: Extract<WorkEntry, { kind: "tool" }> }) {
  const [expanded, setExpanded] = useState(false);
  const { call, fileChange, subagent } = entry;
  const { label, detail, Icon } = describeToolCall(call, fileChange);
  const statusColor =
    call.status === "completed"
      ? "bg-green-500"
      : call.status === "failed"
        ? "bg-red-500"
        : "bg-yellow-500 animate-pulse";

  return (
    <div className="text-xs">
      <button
        type="button"
        className="w-full flex items-center gap-1.5 py-0.5 text-left hover:bg-muted/40 rounded px-1"
        onClick={() => setExpanded((v) => !v)}
      >
        <span
          className={`inline-block h-3 w-3 shrink-0 transition-transform ${
            expanded ? "rotate-90" : ""
          }`}
        >
          <ChevronRight className="h-3 w-3 text-muted-foreground/60" />
        </span>
        <Icon className="h-3 w-3 shrink-0 text-muted-foreground/80" />
        <span className={`w-1.5 h-1.5 rounded-full shrink-0 ${statusColor}`} />
        <span className="font-medium">{label}</span>
        {detail && (
          <span className="text-muted-foreground truncate min-w-0">{detail}</span>
        )}
      </button>
      {expanded && (
        <div className="mt-1 ml-6 space-y-1">
          {fileChange ? (
            <FileChangeDetail change={fileChange} />
          ) : (
            <pre className="text-[11px] bg-muted/40 rounded p-1.5 font-mono overflow-x-auto max-h-48">
              {JSON.stringify(call.args, null, 2)}
            </pre>
          )}
          {call.output && !fileChange && (
            <pre className="text-[11px] bg-muted/30 rounded p-1.5 font-mono overflow-x-auto whitespace-pre-wrap max-h-48">
              {call.output}
            </pre>
          )}
          {call.error && (
            <pre className="text-[11px] text-destructive bg-destructive/10 rounded p-1.5 font-mono whitespace-pre-wrap">
              {call.error}
            </pre>
          )}
          {subagent && (
            <div className="border-l-2 border-muted ml-1 pl-2 space-y-1">
              <div className="text-[10px] text-muted-foreground italic line-clamp-2">
                {subagent.prompt}
              </div>
              {subagent.output && (
                <div className="text-[11px] bg-muted/40 rounded p-1.5 whitespace-pre-wrap">
                  {subagent.output}
                </div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function FileChangeDetail({ change }: { change: FileChangeRecord }) {
  if (change.operation === "delete") {
    return (
      <div className="bg-red-500/10 text-red-300 p-1.5 rounded border border-border text-[11px]">
        (deleted)
      </div>
    );
  }
  if (change.operation === "edit") {
    return (
      <div className="rounded overflow-hidden border border-border text-[11px]">
        {change.before && (
          <div className="bg-red-500/10 text-red-300 whitespace-pre-wrap p-1.5 font-mono">
            {change.before
              .split("\n")
              .map((line) => `- ${line}`)
              .join("\n")}
          </div>
        )}
        {change.after && (
          <div className="bg-green-500/10 text-green-300 whitespace-pre-wrap p-1.5 font-mono">
            {change.after
              .split("\n")
              .map((line) => `+ ${line}`)
              .join("\n")}
          </div>
        )}
      </div>
    );
  }
  // write
  return (
    <pre className="bg-green-500/10 text-green-300 whitespace-pre-wrap p-1.5 rounded border border-border font-mono text-[11px] max-h-48 overflow-auto">
      {change.after ?? ""}
    </pre>
  );
}

export function WorkLogCard(props: Props) {
  const entries = buildEntries(props);
  const [collapsed, setCollapsed] = useState(props.turnCompleted);

  if (entries.length === 0) return null;

  const onlyTools = entries.every((e) => e.kind === "tool");
  const label = onlyTools ? "Tool calls" : "Work log";

  return (
    <div className="rounded-xl border border-border/60 bg-muted/20 px-2 py-1.5 mt-2">
      <button
        type="button"
        onClick={() => setCollapsed((v) => !v)}
        className="w-full flex items-center justify-between gap-2 px-0.5"
      >
        <div className="flex items-center gap-1.5 text-[10px] uppercase tracking-[0.14em] text-muted-foreground">
          {collapsed ? (
            <ChevronRight className="h-3 w-3" />
          ) : (
            <ChevronDown className="h-3 w-3" />
          )}
          <span>
            {label} ({entries.length})
          </span>
        </div>
      </button>
      {!collapsed && (
        <div className="mt-1.5 space-y-0.5">
          {entries.map((entry, index) => {
            if (entry.kind === "tool") {
              return (
                <ToolCallRow key={`tool:${entry.call.callId || index}`} entry={entry} />
              );
            }
            return (
              <div key={`file:${entry.fileChange.callId || index}`} className="text-xs px-1">
                <FileChangeDetail change={entry.fileChange} />
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
