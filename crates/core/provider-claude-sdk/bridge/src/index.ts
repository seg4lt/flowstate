#!/usr/bin/env node
/**
 * Claude Agent SDK Bridge for ZenUI
 *
 * Wraps @anthropic-ai/claude-agent-sdk's query() and forwards SDK message
 * stream events to ZenUI as JSON-line stream events.
 */

import {
  query,
  type SDKMessage,
  type Options,
  type PermissionResult,
  type CanUseTool,
} from '@anthropic-ai/claude-agent-sdk';
import { createInterface } from 'readline';
import { randomUUID } from 'crypto';

type ZenUiMessage = {
  type: string;
  [key: string]: unknown;
};

type DecisionString = 'allow' | 'allow_always' | 'deny' | 'deny_always';

interface PendingPermission {
  resolve: (decision: PermissionResult) => void;
  // Echoed back as updatedInput when the user allows. Without this the
  // SDK would replace the tool's args with {} and the tool would crash
  // (e.g. Bash with command=undefined → "Cannot read properties of
  // undefined (reading 'includes')").
  input: Record<string, unknown>;
}

/**
 * Shape of one `AskUserQuestion` question as emitted by the Claude Agent SDK.
 * Mirrors the public contract at
 * https://code.claude.com/docs/en/agent-sdk/user-input#handle-clarifying-questions
 */
interface AskUserSdkQuestion {
  question: string;
  header?: string;
  options: Array<{ label: string; description?: string }>;
  multiSelect?: boolean;
}

interface PendingQuestion {
  resolve: (result: PermissionResult) => void;
  questions: AskUserSdkQuestion[];
}

interface StructuredAnswer {
  questionId: string;
  optionIds: string[];
  answer: string;
}

const pendingPermissions = new Map<string, PendingPermission>();
const pendingQuestions = new Map<string, PendingQuestion>();

/// Resolve every in-flight permission and question with a denial /
/// dismissal so the SDK's canUseTool callbacks unwind. Called when the
/// turn aborts — without this, awaiting Promises leak forever.
function drainPendingOnAbort(): void {
  for (const [, p] of pendingPermissions) {
    p.resolve({ behavior: 'deny', message: 'Turn aborted' });
  }
  pendingPermissions.clear();
  for (const [, q] of pendingQuestions) {
    q.resolve({ behavior: 'deny', message: 'Turn aborted' });
  }
  pendingQuestions.clear();
}

function writeJson(payload: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(payload) + '\n');
}

function writeStream(payload: Record<string, unknown>): void {
  writeJson({ type: 'stream', ...payload });
}

class ClaudeBridge {
  private cwd: string = process.cwd();
  private model?: string;
  private resumeSessionId?: string;
  private abortController?: AbortController;
  private inFlight = false;

  createSession(cwd: string, model?: string, resumeSessionId?: string): string {
    this.cwd = cwd;
    this.model = model;
    // Hydrate the SDK resume id from persisted state when zenui restarts or
    // this bridge is a fresh respawn. The SDK's `resume:` option on the next
    // send_prompt picks this up and replays the prior conversation.
    if (resumeSessionId) {
      this.resumeSessionId = resumeSessionId;
    }
    const sessionId = `claude-sdk-${randomUUID()}`;
    return sessionId;
  }

  /**
   * Current Claude SDK session id captured from the most recent query, if any.
   * Round-tripped to the Rust adapter so it can be persisted on
   * `session.provider_state.native_thread_id` for cross-restart resume.
   */
  getResumeSessionId(): string | undefined {
    return this.resumeSessionId;
  }

  async sendPrompt(
    prompt: string,
    permissionMode: 'default' | 'acceptEdits' | 'plan' | 'bypassPermissions',
    reasoningEffort?: 'minimal' | 'low' | 'medium' | 'high',
  ): Promise<string> {
    if (this.inFlight) {
      throw new Error('Another turn is already in flight');
    }
    this.inFlight = true;
    this.abortController = new AbortController();

    const canUseTool: CanUseTool = async (
      toolName: string,
      input: Record<string, unknown>,
    ): Promise<PermissionResult> => {
      // AskUserQuestion is Claude's built-in clarifying-question tool. Route it
      // to the question-dialog UI instead of the permission dialog; the return
      // shape is also different — we must pass back `updatedInput: { questions, answers }`
      // where `answers` is keyed by the question text. See
      // https://code.claude.com/docs/en/agent-sdk/user-input#handle-clarifying-questions
      if (toolName === 'AskUserQuestion') {
        const rawQuestions = (input?.questions as AskUserSdkQuestion[] | undefined) ?? [];
        const requestId = randomUUID();
        writeStream({
          event: 'user_question',
          request_id: requestId,
          questions: rawQuestions,
        });
        return new Promise<PermissionResult>((resolve) => {
          pendingQuestions.set(requestId, { resolve, questions: rawQuestions });
        });
      }

      const requestId = randomUUID();
      writeStream({
        event: 'permission_request',
        request_id: requestId,
        tool_name: toolName,
        input,
        suggested: 'allow',
      });
      return new Promise<PermissionResult>((resolve) => {
        pendingPermissions.set(requestId, { resolve, input });
      });
    };

    // Claude SDK Options accepts `maxThinkingTokens` (a numeric budget), not an
    // effort string. Map the runtime's reasoning_effort to a rough token budget;
    // `minimal` disables thinking entirely by omitting the field.
    const thinkingBudget: number | null = (() => {
      switch (reasoningEffort) {
        case 'minimal':
          return 0;
        case 'low':
          return 2048;
        case 'medium':
          return 8000;
        case 'high':
          return 32000;
        default:
          return null;
      }
    })();

    const options: Options = {
      cwd: this.cwd,
      permissionMode,
      canUseTool,
      abortController: this.abortController,
      includePartialMessages: true,
      ...(this.model ? { model: this.model } : {}),
      ...(this.resumeSessionId ? { resume: this.resumeSessionId } : {}),
      ...(thinkingBudget !== null && thinkingBudget > 0
        ? { maxThinkingTokens: thinkingBudget }
        : {}),
    };

    let finalText = '';
    try {
      for await (const message of query({ prompt, options })) {
        const text = this.handleSdkMessage(message);
        if (text != null) finalText = text;
      }
    } catch (err) {
      const e = err as Error;
      if (e.name === 'AbortError') {
        // Drain any in-flight permission/question prompts so their callers
        // (the SDK's canUseTool callbacks) don't sit on a Promise that never
        // resolves. They'll see a deny / dismissal and unwind cleanly.
        drainPendingOnAbort();
        return '[interrupted]';
      }
      throw err;
    } finally {
      this.inFlight = false;
    }
    return finalText;
  }

  /**
   * Translate one SDKMessage into stream events. Returns the final assistant text
   * if this message is a `result` (so the caller can capture canonical output).
   */
  private handleSdkMessage(msg: SDKMessage): string | null {
    switch (msg.type) {
      case 'system': {
        const sub = (msg as { subtype?: string }).subtype;
        if (sub === 'init') {
          const sid = (msg as { session_id?: string }).session_id;
          if (sid) {
            this.resumeSessionId = sid;
            writeStream({ event: 'info', message: `Claude session ${sid}` });
          }
        }
        return null;
      }
      case 'stream_event': {
        // Incremental token streaming. With `includePartialMessages: true`, the SDK
        // emits Anthropic raw stream events. We forward `content_block_delta` chunks
        // as small text/reasoning deltas so the UI updates token-by-token.
        const sm = msg as {
          event: Record<string, unknown>;
          parent_tool_use_id?: string | null;
        };
        const ev = sm.event ?? {};
        const evType = ev.type as string | undefined;
        const parentId = sm.parent_tool_use_id ?? null;
        if (evType === 'content_block_delta') {
          const delta = ev.delta as Record<string, unknown> | undefined;
          const dType = delta?.type as string | undefined;
          if (dType === 'text_delta') {
            const text = (delta?.text as string) ?? '';
            if (text.length > 0) {
              if (parentId) {
                writeStream({
                  event: 'subagent_event',
                  agent_id: parentId,
                  nested_event: { role: 'assistant', text },
                });
              } else {
                writeStream({ event: 'text_delta', delta: text });
              }
            }
          } else if (dType === 'thinking_delta') {
            const thinking = (delta?.thinking as string) ?? '';
            if (thinking.length > 0) {
              writeStream({ event: 'reasoning_delta', delta: thinking });
            }
          }
        }
        return null;
      }
      case 'assistant': {
        const m = msg as {
          message: { content: Array<Record<string, unknown>> };
          parent_tool_use_id?: string | null;
        };
        // Text and thinking blocks were already streamed via `stream_event`, so
        // skip them here to avoid duplicating the full message body. We still
        // process `tool_use` blocks because those only arrive complete.
        for (const block of m.message.content) {
          const t = block.type as string;
          if (t === 'tool_use') {
            const callId = block.id as string | undefined;
            const name = block.name as string | undefined;
            // Anthropic guarantees both fields on tool_use blocks, but
            // skipping defensively keeps a corrupted SDK message from
            // propagating empty IDs that break tool result correlation
            // downstream. Log so the malformed block is visible in debug.
            if (!callId || !name) {
              console.error(
                `[bridge] skipping malformed tool_use block (id=${callId} name=${name})`,
              );
              continue;
            }
            const input = (block.input as Record<string, unknown>) ?? {};
            writeStream({
              event: 'tool_started',
              call_id: callId,
              name,
              args: input,
            });

            // Structured file-change extraction for Write/Edit/Delete tools.
            if (name === 'Write') {
              writeStream({
                event: 'file_change',
                call_id: callId,
                path: (input.file_path as string) ?? '',
                operation: 'write',
                after: (input.content as string) ?? '',
              });
            } else if (name === 'Edit') {
              writeStream({
                event: 'file_change',
                call_id: callId,
                path: (input.file_path as string) ?? '',
                operation: 'edit',
                before: (input.old_string as string) ?? '',
                after: (input.new_string as string) ?? '',
              });
            }

            // Subagent dispatch
            if (name === 'Task' || name === 'Agent') {
              writeStream({
                event: 'subagent_started',
                parent_call_id: callId,
                agent_id: callId,
                agent_type: (input.subagent_type as string) ?? 'general-purpose',
                prompt: (input.prompt as string) ?? '',
              });
            }

            // Plan mode tool: ExitPlanMode emits the plan.
            if (name === 'ExitPlanMode') {
              const raw = (input.plan as string) ?? '';
              writeStream({
                event: 'plan_proposed',
                plan_id: callId,
                title: 'Proposed plan',
                steps: parsePlanSteps(raw),
                raw,
              });
            }
          }
        }
        return null;
      }
      case 'user': {
        const m = msg as {
          message: { content: Array<Record<string, unknown>> };
          parent_tool_use_id?: string | null;
        };
        for (const block of m.message.content) {
          if (block.type === 'tool_result') {
            const cid = (block.tool_use_id as string) ?? '';
            const raw = block.content as unknown;
            const output =
              typeof raw === 'string' ? raw : JSON.stringify(raw);
            const isError = (block.is_error as boolean) === true;
            writeStream({
              event: 'tool_completed',
              call_id: cid,
              output,
              ...(isError ? { error: 'tool returned an error' } : {}),
            });

            // If this user message is nested under a parent tool, mark subagent completion.
            if (m.parent_tool_use_id) {
              writeStream({
                event: 'subagent_completed',
                agent_id: m.parent_tool_use_id,
                output,
                ...(isError ? { error: 'tool error' } : {}),
              });
            }
          }
        }
        return null;
      }
      case 'result': {
        const r = msg as {
          subtype: string;
          result?: string;
          session_id?: string;
        };
        if (r.session_id) this.resumeSessionId = r.session_id;
        if (r.subtype === 'success') {
          return r.result ?? '';
        }
        return null;
      }
      default:
        return null;
    }
  }

  answerPermission(requestId: string, decision: DecisionString): void {
    const p = pendingPermissions.get(requestId);
    if (!p) return;
    pendingPermissions.delete(requestId);
    const allow = decision === 'allow' || decision === 'allow_always';
    if (allow) {
      // Echo the original input — passing {} would replace the tool's
      // args with an empty object and crash inside the tool handler.
      p.resolve({ behavior: 'allow', updatedInput: p.input });
    } else {
      p.resolve({ behavior: 'deny', message: 'User denied' });
    }
  }

  answerQuestion(requestId: string, answers: StructuredAnswer[]): void {
    const pending = pendingQuestions.get(requestId);
    if (!pending) return;
    pendingQuestions.delete(requestId);

    // Claude expects `updatedInput: { questions, answers: { "<question text>": "<value>" } }`.
    // We synthesized `questionId` as the question's array index on the Rust side, so
    // look up each answer's original question text here.
    const answerMap: Record<string, string> = {};
    for (const a of answers) {
      const idx = Number(a.questionId);
      const q = pending.questions[idx];
      if (!q) continue;
      answerMap[q.question] = a.answer;
    }

    pending.resolve({
      behavior: 'allow',
      updatedInput: {
        questions: pending.questions,
        answers: answerMap,
      },
    });
  }

  cancelQuestion(requestId: string): void {
    const pending = pendingQuestions.get(requestId);
    if (!pending) return;
    pendingQuestions.delete(requestId);
    // Resolve the pending canUseTool promise with deny so the SDK reports the
    // tool call as user-denied. The model sees the message string and can
    // proceed without the clarifying answer.
    pending.resolve({
      behavior: 'deny',
      message: 'User cancelled the clarifying question',
    });
  }

  interrupt(): void {
    if (this.abortController) {
      try {
        this.abortController.abort();
      } catch {
        // ignore
      }
    }
  }

  /**
   * List the models the Claude Agent SDK reports as supported. Cheapest path:
   * call query() with a noop prompt, abort immediately, then read
   * supportedModels() — internally that just returns the cached init response.
   */
  async listModels(): Promise<Array<{ value: string; label: string }>> {
    const abortController = new AbortController();
    const q = query({
      prompt: 'noop',
      options: {
        cwd: this.cwd,
        abortController,
      },
    });
    try {
      const models = await q.supportedModels();
      return models.map((m) => ({
        value: m.value,
        label: m.displayName ?? m.value,
      }));
    } finally {
      try {
        abortController.abort();
      } catch {
        // ignore
      }
    }
  }
}

function parsePlanSteps(raw: string): Array<{ title: string; detail?: string }> {
  if (!raw) return [];
  const lines = raw.split('\n');
  const steps: Array<{ title: string; detail?: string }> = [];
  for (const line of lines) {
    const trimmed = line.trim();
    const match = trimmed.match(/^(?:[-*]|\d+\.)\s+(.*)$/);
    if (match) {
      steps.push({ title: match[1] });
    }
  }
  return steps;
}

async function main(): Promise<void> {
  const bridge = new ClaudeBridge();

  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  // Send ready signal once the SDK module is loaded.
  writeJson({ type: 'ready' });
  console.error('[claude-bridge] Ready for commands');

  // Track the current send_prompt promise so we can keep accepting `answer_permission`
  // and other messages while it's in flight.
  let promptInFlight: Promise<void> | null = null;

  rl.on('line', (line: string) => {
    let msg: ZenUiMessage;
    try {
      msg = JSON.parse(line) as ZenUiMessage;
    } catch (err) {
      writeJson({
        type: 'error',
        error: `Invalid JSON: ${(err as Error).message}`,
      });
      return;
    }

    switch (msg.type) {
      case 'create_session': {
        const cwd = (msg.cwd as string) ?? process.cwd();
        const model = msg.model as string | undefined;
        const resumeSessionId = msg.resume_session_id as string | undefined;
        try {
          const sessionId = bridge.createSession(cwd, model, resumeSessionId);
          writeJson({ type: 'session_created', session_id: sessionId });
        } catch (err) {
          writeJson({
            type: 'error',
            error: (err as Error).message,
          });
        }
        break;
      }

      case 'send_prompt': {
        const prompt = msg.prompt as string;
        const mode = (msg.permission_mode as
          | 'default'
          | 'acceptEdits'
          | 'plan'
          | 'bypassPermissions') ?? 'acceptEdits';
        const effort = msg.reasoning_effort as
          | 'minimal'
          | 'low'
          | 'medium'
          | 'high'
          | undefined;
        promptInFlight = (async () => {
          try {
            const output = await bridge.sendPrompt(prompt, mode, effort);
            writeJson({
              type: 'response',
              output,
              session_id: bridge.getResumeSessionId() ?? null,
            });
          } catch (err) {
            writeJson({
              type: 'error',
              error: (err as Error).message,
            });
          } finally {
            promptInFlight = null;
          }
        })();
        break;
      }

      case 'answer_permission': {
        bridge.answerPermission(
          msg.request_id as string,
          msg.decision as DecisionString,
        );
        break;
      }

      case 'answer_question': {
        bridge.answerQuestion(
          msg.request_id as string,
          (msg.answers as StructuredAnswer[]) ?? [],
        );
        break;
      }

      case 'cancel_question': {
        bridge.cancelQuestion(msg.request_id as string);
        break;
      }

      case 'interrupt': {
        bridge.interrupt();
        writeJson({ type: 'interrupted' });
        break;
      }

      case 'list_models': {
        (async () => {
          try {
            const models = await bridge.listModels();
            writeJson({ type: 'models', models });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `list_models failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'shutdown': {
        console.error('[claude-bridge] Shutdown requested');
        process.exit(0);
        break;
      }

      default:
        writeJson({
          type: 'error',
          error: `Unknown message type: ${msg.type}`,
        });
    }
  });

  rl.on('close', async () => {
    if (promptInFlight) {
      try {
        await promptInFlight;
      } catch {
        // ignore
      }
    }
    process.exit(0);
  });
}

main().catch((err) => {
  console.error('[claude-bridge] Fatal error:', err);
  process.exit(1);
});
