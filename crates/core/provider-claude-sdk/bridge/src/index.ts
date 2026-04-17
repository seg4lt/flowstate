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
  type Query,
  type PermissionMode as SdkPermissionMode,
} from '@anthropic-ai/claude-agent-sdk';
import { createInterface } from 'readline';
import { randomUUID } from 'crypto';
import { existsSync } from 'fs';
import { delimiter as pathDelimiter, join as joinPath } from 'path';
import { homedir } from 'os';

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

/**
 * Optional override: try to resolve the user's locally-installed
 * `claude` CLI on PATH and hand it to the Claude Agent SDK via
 * `pathToClaudeCodeExecutable`. The whole thing is opportunistic —
 * if no local `claude` is found we leave the option unset and the
 * SDK transparently falls back to the binary it bundles inside
 * `@anthropic-ai/claude-agent-sdk` (extracted via cli.js / embed.js
 * at runtime), so users who have never installed Claude Code locally
 * still get a working bridge with zero setup.
 *
 * Why prefer the local install when present:
 *   - Picks up the user's existing Claude Code login automatically
 *     (no separate `ANTHROPIC_API_KEY` plumbing required)
 *   - Honors their MCP server configuration, settings, and any
 *     globally-configured tools
 *   - Tracks newer Claude Code releases without re-bundling the SDK
 *
 * Resolution is a pure-Node PATH walk (using `process.env.PATH` and
 * `path.delimiter`, with `PATHEXT` on Windows) plus a curated list
 * of well-known install locations across Linux, macOS, and Windows.
 * No shell, no `which` / `where` subprocess, no extra npm deps.
 *
 * Computed once at module load and cached — the binary location
 * doesn't change between turns within a single bridge process.
 */
function resolveLocalClaudeBinary(): string | null {
  const isWindows = process.platform === 'win32';
  const exeExtensions = isWindows
    ? (process.env.PATHEXT ?? '.COM;.EXE;.BAT;.CMD')
        .split(';')
        .filter((e) => e.length > 0)
    : [''];
  // Always include the bare name on every platform, in case the user
  // installed a shim script without an extension on Windows.
  if (!exeExtensions.includes('')) {
    exeExtensions.unshift('');
  }

  const pathEntries = (process.env.PATH ?? '')
    .split(pathDelimiter)
    .filter((entry) => entry.length > 0);

  for (const dir of pathEntries) {
    for (const ext of exeExtensions) {
      const candidate = joinPath(dir, `claude${ext}`);
      try {
        if (existsSync(candidate)) return candidate;
      } catch {
        // Permission errors on individual entries shouldn't abort the walk.
      }
    }
  }

  // Fallback: well-known install locations across the three OSes.
  // Tried only when PATH lookup misses (e.g. host process didn't
  // forward PATH, or `claude` is installed somewhere unusual).
  const home = homedir();
  const fallbackPaths: string[] = isWindows
    ? [
        joinPath(home, 'AppData', 'Local', 'Programs', 'claude', 'claude.exe'),
        joinPath(home, 'AppData', 'Roaming', 'npm', 'claude.cmd'),
        'C:\\Program Files\\Claude\\claude.exe',
      ]
    : [
        joinPath(home, '.local', 'bin', 'claude'),
        '/opt/homebrew/bin/claude',
        '/usr/local/bin/claude',
        '/home/linuxbrew/.linuxbrew/bin/claude',
        '/usr/bin/claude',
      ];

  for (const candidate of fallbackPaths) {
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // skip
    }
  }

  return null;
}

const RESOLVED_LOCAL_CLAUDE_PATH: string | null = resolveLocalClaudeBinary();
if (RESOLVED_LOCAL_CLAUDE_PATH) {
  console.error(
    `[bridge] Using local claude CLI at: ${RESOLVED_LOCAL_CLAUDE_PATH}`,
  );
} else {
  console.error(
    "[bridge] No local claude CLI found; falling back to the SDK's bundled binary",
  );
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
  /**
   * The SDK session id to `resume:` from on the NEXT query. Only updated
   * from `result` messages — i.e. turns the SDK has committed to its
   * on-disk session store. Persisted back to Rust as
   * `provider_state.native_thread_id`.
   *
   * Crucially, we do NOT overwrite this from `system.init` mid-turn:
   * the init id represents a session the SDK hasn't yet committed, and
   * if the turn is interrupted that id is never finalised. Resuming
   * from it would give Claude an empty or stale context — exactly the
   * steering-mode context-loss bug this two-phase scheme prevents.
   * See `pendingInitSessionId` below.
   */
  private resumeSessionId?: string;
  /**
   * The init session id of the in-flight query, held aside until the
   * matching `result` arrives. Promoted into `resumeSessionId` on
   * successful completion; discarded on abort or any other unwind so
   * we never ship a never-committed id back to the host.
   */
  private pendingInitSessionId?: string;
  private abortController?: AbortController;
  private inFlight = false;
  /**
   * Live handle to the SDK Query object for the in-flight turn, if any.
   * The SDK exposes mid-turn control methods like `setPermissionMode` and
   * `interrupt` on this object — we hold onto it so the host can flip
   * the active permission mode (e.g. when the user approves an
   * ExitPlanMode and picks "Accept Edits") without restarting the turn.
   * Cleared in the finally block of `sendPrompt`.
   */
  private activeQuery?: Query;
  /**
   * Current effective permission mode for the in-flight turn. Seeded
   * from the `permissionMode` parameter at the start of each
   * `sendPrompt`, then kept in sync with every subsequent mode
   * change — whether the host pushed an `update_permission_mode` RPC
   * (`setPermissionMode` method below) or the user approved an
   * ExitPlanMode with a new mode (`answerPermission` with a mode
   * override, which rides the SDK's `updatedPermissions: [{setMode}]`
   * mechanism).
   *
   * Read by `canUseTool` on every tool invocation to decide whether
   * bypass mode should short-circuit the prompt. The previous
   * implementation read the closure-captured `permissionMode`
   * parameter, which froze at turn start and therefore failed the
   * bypass check for tools AFTER an in-turn mode change — the
   * user's "click Bypass Permissions in the plan-exit dialog but
   * still get prompted for the next Bash" bug.
   */
  private livePermissionMode?: SdkPermissionMode;

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

  /** Mid-session model switch. Updates the model used by subsequent
   *  `query()` calls without restarting the bridge process. */
  setModel(model: string): void {
    this.model = model;
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
    images: Array<{ media_type: string; data_base64: string }> = [],
  ): Promise<string> {
    if (this.inFlight) {
      throw new Error('Another turn is already in flight');
    }
    this.inFlight = true;
    this.abortController = new AbortController();
    // Seed the live-mode tracker for this turn. Every subsequent
    // mid-turn mode change updates this field so canUseTool reads
    // the CURRENT mode, not the frozen turn-start one.
    this.livePermissionMode = permissionMode;

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

      // Bypass mode: the user explicitly opted out of permission
      // prompting. Resolve every non-question tool call as-is
      // without round-tripping through the host. Without this the
      // SDK would still call canUseTool for mutating tools
      // (Bash/Write/Edit/...), we'd emit permission_request, and the
      // UI would show a dialog the user explicitly said they didn't
      // want. See also the runtime-core safety net which auto-answers
      // any permission_request that slips through for a bypass turn
      // — this short-circuit is the primary fix; the safety net
      // covers other provider adapters.
      //
      // Reads `this.livePermissionMode`, not the closure-captured
      // `permissionMode` parameter, so an in-turn mode change
      // (ExitPlanMode → Bypass, toolbar toggle → update_permission_mode)
      // takes effect for every tool call that follows.
      if (this.livePermissionMode === 'bypassPermissions') {
        return { behavior: 'allow', updatedInput: input };
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

    // In plan mode the model is investigating, not changing anything.
    // Prompting for every Read / Grep / Glob / WebSearch / TodoWrite is
    // pure friction -- those calls can't damage anything and the user
    // already opted into "let the model look around" by entering plan
    // mode. Pass them via the SDK's built-in allowedTools so canUseTool
    // is never even invoked for them. ExitPlanMode is intentionally
    // NOT in the list -- that's THE prompt that matters in plan mode
    // (the plan approval). Bash, Write, Edit, NotebookEdit, and any
    // unknown tool keep going through canUseTool because they can
    // mutate state (Bash can run rm/git push/etc., the rest are
    // explicitly modifying). Task/Agent dispatches a sub-agent which
    // inherits the same plan-mode constraints, so we auto-allow it
    // too -- otherwise the user would get prompted just to let the
    // model spawn an investigator.
    const planModeAllowedTools = [
      'Read',
      'Grep',
      'Glob',
      'WebFetch',
      'WebSearch',
      'TodoWrite',
      'Task',
      'Agent',
    ];

    const options: Options = {
      cwd: this.cwd,
      permissionMode,
      canUseTool,
      abortController: this.abortController,
      includePartialMessages: true,
      ...(permissionMode === 'plan'
        ? { allowedTools: planModeAllowedTools }
        : {}),
      ...(this.model ? { model: this.model } : {}),
      ...(this.resumeSessionId ? { resume: this.resumeSessionId } : {}),
      ...(thinkingBudget !== null && thinkingBudget > 0
        ? { maxThinkingTokens: thinkingBudget }
        : {}),
      // Optional: prefer the user's local Claude Code install over
      // the SDK's bundled binary when one is on PATH. Spread is
      // empty (no key set) when no local install was found, so the
      // SDK transparently uses its own embedded executable.
      ...(RESOLVED_LOCAL_CLAUDE_PATH
        ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
        : {}),
    };

    let finalText = '';
    // When the user pasted one or more images we have to use the
    // `query({ prompt: AsyncIterable<SDKUserMessage>, … })` form instead
    // of the plain string path — Claude Agent SDK only accepts
    // multimodal `content` arrays via that channel. The async iterator
    // yields exactly one user message whose `content` is text + image
    // blocks, then closes; the SDK pumps it like any other turn.
    type SdkPromptInput = Parameters<typeof query>[0]['prompt'];
    type SdkImageMediaType =
      | 'image/jpeg'
      | 'image/png'
      | 'image/gif'
      | 'image/webp';
    const promptInput: SdkPromptInput = images.length === 0
      ? prompt
      : (async function* userMessages() {
          yield {
            type: 'user' as const,
            message: {
              role: 'user' as const,
              content: [
                { type: 'text' as const, text: prompt },
                ...images.map((img) => ({
                  type: 'image' as const,
                  source: {
                    type: 'base64' as const,
                    media_type: img.media_type as SdkImageMediaType,
                    data: img.data_base64,
                  },
                })),
              ],
            },
            parent_tool_use_id: null,
            session_id: '',
          };
        })();
    const q = query({ prompt: promptInput, options });
    this.activeQuery = q;
    try {
      for await (const message of q) {
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
        // Discard the pending init id — the SDK never committed this
        // session via `result`, so resuming from it on the next turn
        // would give the model an empty/stale context. `resumeSessionId`
        // remains pinned at the last COMPLETED turn's id, which is
        // what steering-mode needs to preserve conversation history.
        this.pendingInitSessionId = undefined;
        return '[interrupted]';
      }
      throw err;
    } finally {
      this.inFlight = false;
      this.activeQuery = undefined;
      // Belt-and-suspenders: on any unwind path (abort handled above,
      // unexpected throw re-raised, or normal completion where result
      // already promoted the id), ensure we don't carry a stale
      // pending id into the next turn.
      this.pendingInitSessionId = undefined;
    }
    return finalText;
  }

  /**
   * Forward a runtime permission-mode change to the in-flight SDK query.
   * No-op if no turn is currently running. The host calls this after a
   * user approves an ExitPlanMode and picks a new mode for the rest of
   * the turn.
   */
  async setPermissionMode(mode: SdkPermissionMode): Promise<void> {
    if (!this.activeQuery) return;
    await this.activeQuery.setPermissionMode(mode);
    // Keep our canUseTool view in sync with the SDK's internal
    // state — without this the bypass short-circuit would still
    // read the stale turn-start mode for the rest of the turn.
    this.livePermissionMode = mode;
  }

  /**
   * Translate one SDKMessage into stream events. Returns the final assistant text
   * if this message is a `result` (so the caller can capture canonical output).
   */
  private handleSdkMessage(msg: SDKMessage): string | null {
    // Non-delta SDK message dispatch log. Stream_event messages are
    // high-volume (one per token) so we suppress them to keep the log
    // readable; everything else — assistant, user, result, system,
    // tool_progress, tool_use_summary — gets a single line so the
    // "did a user/tool_result message arrive after the permission
    // answer" question is answerable from the log alone.
    if (msg.type !== 'stream_event') {
      const subtype =
        (msg as { subtype?: string }).subtype !== undefined
          ? `.${(msg as { subtype?: string }).subtype}`
          : '';
      console.error(`[bridge] sdk msg type=${msg.type}${subtype}`);
    }
    switch (msg.type) {
      case 'system': {
        const sub = (msg as { subtype?: string }).subtype;
        if (sub === 'init') {
          const init = msg as { session_id?: string; model?: string };
          if (init.session_id) {
            // Stash as pending, not promoted. The SDK hasn't committed
            // this session to its store yet — it will on `result`. If
            // the turn is aborted mid-stream (steering), we must NOT
            // ship this id back as the resume target because the SDK
            // can't resume from a session it never finalised.
            this.pendingInitSessionId = init.session_id;
            writeStream({ event: 'info', message: `Claude session ${init.session_id}` });
          }
          // Surface the model the SDK actually resolved. If the requested
          // model string is rejected or remapped, this is the only place
          // the truth shows up — the bridge sends what the Rust adapter
          // asked for, but the SDK/CLI may silently substitute.
          writeStream({
            event: 'info',
            message: `Claude model: requested=${this.model ?? '<default>'} resolved=${init.model ?? '<unknown>'}`,
          });
          // Emit the resolved model as a structured event too, so the
          // Rust adapter can upgrade `session.summary.model` to match
          // what the SDK actually runs. Without this the UI model
          // selector fails to highlight the active entry whenever the
          // stored value is an alias (`sonnet`) but the dropdown list
          // carries pinned ids (`claude-sonnet-4-5-<date>`) returned
          // by `supportedModels()`.
          if (init.model) {
            writeStream({ event: 'model_resolved', model: init.model });
          }
        }
        return null;
      }
      case 'rate_limit_event': {
        // Claude subscription rate-limit snapshot. Maps
        // Anthropic's bucket names to human-readable labels inside
        // the bridge so the shared provider-api RateLimitInfo stays
        // provider-agnostic. Drops events without a bucket type —
        // those carry no actionable info.
        const rl = msg as {
          rate_limit_info: {
            status: 'allowed' | 'allowed_warning' | 'rejected';
            rateLimitType?:
              | 'five_hour'
              | 'seven_day'
              | 'seven_day_opus'
              | 'seven_day_sonnet'
              | 'overage';
            utilization?: number;
            resetsAt?: number;
            isUsingOverage?: boolean;
          };
        };
        const info = rl.rate_limit_info;
        if (!info.rateLimitType || info.utilization == null) return null;
        const labels: Record<string, string> = {
          five_hour: '5-hour limit',
          seven_day: 'Weekly · all models',
          seven_day_opus: 'Weekly · Opus',
          seven_day_sonnet: 'Weekly · Sonnet',
          overage: 'Overage',
        };
        writeStream({
          event: 'rate_limit_update',
          rate_limit_info: {
            bucket: info.rateLimitType,
            label: labels[info.rateLimitType] ?? info.rateLimitType,
            status: info.status,
            utilization: info.utilization,
            resetsAt: info.resetsAt ?? null,
            isUsingOverage: info.isUsingOverage ?? false,
          },
        });
        return null;
      }
      case 'stream_event': {
        // Incremental token streaming. With `includePartialMessages: true`, the SDK
        // emits Anthropic raw stream events. We forward `content_block_delta` chunks
        // as small text/reasoning deltas so the UI updates token-by-token.
        const sm = msg as unknown as {
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
        const m = msg as unknown as {
          message: { content: unknown };
          parent_tool_use_id?: string | null;
        };
        const rawContent = m.message?.content;
        if (!Array.isArray(rawContent)) {
          console.error(
            `[bridge] assistant message content is not an array: type=${typeof rawContent}`,
          );
          return null;
        }
        // Assistant messages from sub-agents (spawned via the Task/Agent
        // tool) carry a non-null parent_tool_use_id pointing back at the
        // spawner's call_id. Propagate it onto every tool_started we
        // forward from this message so the frontend can group tool calls
        // by the agent that actually ran them.
        const parentToolUseId = m.parent_tool_use_id ?? undefined;
        const blockTypes = rawContent
          .map((b) => (b as { type?: string }).type ?? '?')
          .join(',');
        console.error(
          `[bridge] assistant blocks=[${blockTypes}] parent=${parentToolUseId ?? '-'}`,
        );
        // Text and thinking blocks were already streamed via `stream_event`, so
        // skip them here to avoid duplicating the full message body. We still
        // process `tool_use` blocks because those only arrive complete.
        for (const block of rawContent as Array<Record<string, unknown>>) {
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
              ...(parentToolUseId ? { parent_call_id: parentToolUseId } : {}),
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

            // Plan mode tools: ExitPlanMode emits the plan,
            // EnterPlanMode signals the model wants plan mode.
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
            if (name === 'EnterPlanMode') {
              writeStream({
                event: 'plan_mode_entered',
                call_id: callId,
              });
            }
          }
        }
        return null;
      }
      case 'user': {
        const m = msg as unknown as {
          message: { content: unknown };
          parent_tool_use_id?: string | null;
        };
        const rawContent = m.message?.content;
        if (!Array.isArray(rawContent)) {
          console.error(
            `[bridge] user message content is not an array: type=${typeof rawContent}`,
          );
          return null;
        }
        const blockSummary = rawContent
          .map((b) => {
            const bb = b as { type?: string; tool_use_id?: string };
            return bb.type === 'tool_result'
              ? `tool_result(${bb.tool_use_id ?? '?'})`
              : (bb.type ?? '?');
          })
          .join(',');
        console.error(`[bridge] user blocks=[${blockSummary}]`);
        for (const block of rawContent as Array<Record<string, unknown>>) {
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
          usage?: {
            input_tokens: number;
            output_tokens: number;
            cache_creation_input_tokens?: number | null;
            cache_read_input_tokens?: number | null;
          };
          modelUsage?: Record<
            string,
            { contextWindow?: number; costUSD?: number }
          >;
          total_cost_usd?: number;
          duration_ms?: number;
        };
        // The turn completed cleanly — the SDK committed this session
        // to its store, so it's now safe to promote the id as the
        // resume target for the next query. Clear the pending id so an
        // abort later in this bridge's lifetime can't accidentally
        // resurrect it.
        if (r.session_id) {
          this.resumeSessionId = r.session_id;
          this.pendingInitSessionId = undefined;
        }
        // Forward token usage before returning the output text so the
        // runtime-core drain loop sees a TurnUsage event before the
        // turn finalises. Maps Anthropic's SDK field names onto the
        // provider-agnostic TokenUsage shape. Picks the first key
        // in modelUsage as the source of truth for contextWindow —
        // a single Flowstate turn only runs on one model at a time.
        if (r.usage) {
          const modelKey = r.modelUsage
            ? Object.keys(r.modelUsage)[0]
            : undefined;
          const mu = modelKey ? r.modelUsage![modelKey] : undefined;
          writeStream({
            event: 'turn_usage',
            usage: {
              inputTokens: r.usage.input_tokens,
              outputTokens: r.usage.output_tokens,
              cacheWriteTokens: r.usage.cache_creation_input_tokens ?? null,
              cacheReadTokens: r.usage.cache_read_input_tokens ?? null,
              contextWindow: mu?.contextWindow ?? null,
              totalCostUsd: r.total_cost_usd ?? null,
              durationMs: r.duration_ms ?? null,
              model: modelKey ?? null,
            },
          });
        }
        if (r.subtype === 'success') {
          return r.result ?? '';
        }
        return null;
      }
      default:
        return null;
    }
  }

  answerPermission(
    requestId: string,
    decision: DecisionString,
    permissionMode?: SdkPermissionMode,
  ): void {
    const p = pendingPermissions.get(requestId);
    // stderr is teed into the Rust daemon log via the adapter's stderr
    // reader, so this single line is the authoritative "did the bridge
    // receive the answer for this request_id" signal when diagnosing a
    // stuck-pending tool card.
    console.error(
      `[bridge] answer_permission request_id=${requestId} decision=${decision} mode=${permissionMode ?? '-'} found=${!!p}`,
    );
    if (!p) return;
    pendingPermissions.delete(requestId);
    const allow = decision === 'allow' || decision === 'allow_always';
    if (allow) {
      // Echo the original input — passing {} would replace the tool's
      // args with an empty object and crash inside the tool handler.
      // If a mode override was supplied (used by the host's plan-exit
      // approve flow), include it in updatedPermissions so the SDK
      // applies the mode change as part of accepting the tool call.
      // Without this, switching mode after an ExitPlanMode approval
      // doesn't make the model continue executing in the new mode
      // within the same turn — the SDK's plan-mode constraints win.
      const result: PermissionResult = {
        behavior: 'allow',
        updatedInput: p.input,
      };
      if (permissionMode) {
        (result as PermissionResult & {
          updatedPermissions?: Array<{
            type: 'setMode';
            mode: SdkPermissionMode;
            destination: 'session';
          }>;
        }).updatedPermissions = [
          { type: 'setMode', mode: permissionMode, destination: 'session' },
        ];
        // Mirror the mode change into our canUseTool view. The SDK
        // applies `updatedPermissions.setMode` internally at the
        // moment this promise resolves, but it has no getter we can
        // poll — without shadowing it here the next tool invocation
        // would see the stale turn-start mode and fail the bypass
        // short-circuit. This is the exact bug the user reported:
        // ExitPlanMode → Bypass Permissions, then the next Bash
        // still prompts.
        this.livePermissionMode = permissionMode;
      }
      p.resolve(result);
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
        // Same opportunistic local-claude override as the main turn
        // query: use the user's install when present, leave the
        // option unset otherwise.
        ...(RESOLVED_LOCAL_CLAUDE_PATH
          ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
          : {}),
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

  /**
   * Enumerate the slash commands, sub-agents, and MCP servers the
   * Claude Agent SDK reports for a given cwd. Same ephemeral-query
   * trick as `listModels`: spawn with a noop prompt so init fires,
   * read the cached `supportedCommands` / `supportedAgents` /
   * `mcpServerStatus`, then abort. The aborted query doesn't round-trip
   * to the Claude API, so this is cheap enough to call on popup open.
   *
   * `cwd` is explicit (not `this.cwd`) so the caller can probe any
   * session's working directory without first calling `create_session`.
   */
  async listCapabilities(
    cwd: string,
    model?: string,
  ): Promise<{
    commands: Array<{ name: string; description: string; argumentHint?: string }>;
    agents: Array<{ name: string; description: string; model?: string }>;
    mcpServers: Array<{ name: string; status: string; scope?: string; error?: string }>;
  }> {
    const abortController = new AbortController();
    const q = query({
      prompt: 'noop',
      options: {
        cwd,
        abortController,
        ...(model ? { model } : {}),
        ...(RESOLVED_LOCAL_CLAUDE_PATH
          ? { pathToClaudeCodeExecutable: RESOLVED_LOCAL_CLAUDE_PATH }
          : {}),
      },
    });
    try {
      const [commands, agents, mcpServers] = await Promise.all([
        q.supportedCommands(),
        q.supportedAgents(),
        q.mcpServerStatus(),
      ]);
      return {
        commands: commands.map((c) => ({
          name: c.name,
          description: c.description,
          argumentHint: c.argumentHint,
        })),
        agents: agents.map((a) => ({
          name: a.name,
          description: a.description,
          model: a.model,
        })),
        mcpServers: mcpServers.map((m) => ({
          name: m.name,
          status: m.status,
          scope: m.scope,
          error: m.error,
        })),
      };
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
        const images = (msg.images as
          | Array<{ media_type: string; data_base64: string }>
          | undefined) ?? [];
        promptInFlight = (async () => {
          try {
            const output = await bridge.sendPrompt(prompt, mode, effort, images);
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
        const mode = msg.permission_mode as SdkPermissionMode | undefined;
        bridge.answerPermission(
          msg.request_id as string,
          msg.decision as DecisionString,
          mode,
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

      case 'set_permission_mode': {
        // Mid-turn permission switch. The Rust runtime sends this when
        // the user picks "Approve & Auto-edit" (etc.) on an ExitPlanMode
        // approval. Map our wire mode names to the SDK's enum.
        const mode = msg.permission_mode as
          | 'default'
          | 'acceptEdits'
          | 'plan'
          | 'bypassPermissions';
        (async () => {
          try {
            await bridge.setPermissionMode(mode);
            writeJson({ type: 'permission_mode_set', mode });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `set_permission_mode failed: ${(err as Error).message}`,
            });
          }
        })();
        break;
      }

      case 'set_model': {
        // Mid-session model switch. Updates the model used by subsequent
        // query() calls. Synchronous — no async work needed.
        const model = msg.model as string;
        bridge.setModel(model);
        writeJson({ type: 'model_set', model });
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

      case 'list_capabilities': {
        // Enumerate slash commands / sub-agents / MCP servers for a
        // given cwd. Called from the Rust adapter's
        // session_command_catalog override. Independent of any active
        // session — we spawn an ephemeral query, read init, abort.
        const cwd = (msg.cwd as string) ?? process.cwd();
        const model = msg.model as string | undefined;
        (async () => {
          try {
            const caps = await bridge.listCapabilities(cwd, model);
            writeJson({
              type: 'capabilities',
              commands: caps.commands,
              agents: caps.agents,
              mcp_servers: caps.mcpServers,
            });
          } catch (err) {
            writeJson({
              type: 'error',
              error: `list_capabilities failed: ${(err as Error).message}`,
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
