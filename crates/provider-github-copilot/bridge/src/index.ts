#!/usr/bin/env node
/**
 * GitHub Copilot SDK Bridge for ZenUI
 *
 * This bridge uses the official @github/copilot-sdk to communicate
 * with the GitHub Copilot CLI, forwarding streaming events as JSON lines.
 */

import { CopilotClient, approveAll } from '@github/copilot-sdk';
import { createInterface } from 'readline';

// ZenUI protocol types
interface ZenUiMessage {
  type: string;
  [key: string]: unknown;
}

/** Write a stream event JSON line to stdout. */
function writeStream(payload: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify({ type: 'stream', ...payload }) + '\n');
}

class CopilotBridge {
  private client: CopilotClient | null = null;
  private session: any = null;

  async start(): Promise<void> {
    console.error('[bridge] Starting GitHub Copilot SDK Bridge...');

    // Find system copilot CLI
    const copilotPaths = [
      '/opt/homebrew/bin/copilot',
      '/usr/local/bin/copilot',
      '/home/linuxbrew/.linuxbrew/bin/copilot',
      'copilot',
    ];

    let copilotPath = 'copilot';
    for (const path of copilotPaths) {
      try {
        const { execSync } = await import('child_process');
        execSync(`${path} --version`, { stdio: 'ignore' });
        copilotPath = path;
        console.error(`[bridge] Found copilot at: ${path}`);
        break;
      } catch {
        // Continue
      }
    }

    // Create client with system CLI
    this.client = new CopilotClient({
      useStdio: true,
      cliPath: copilotPath,
      logLevel: 'info',
    });

    console.error('[bridge] Connecting to Copilot CLI...');
    await this.client.start();
    console.error('[bridge] Connected to Copilot CLI');
  }

  async createSession(cwd: string, model?: string): Promise<string> {
    if (!this.client) {
      throw new Error('Client not started');
    }

    const selectedModel = model ?? 'gpt-4o';
    console.error(`[bridge] Creating session in: ${cwd} (model: ${selectedModel})`);

    this.session = await this.client.createSession({
      model: selectedModel,
      onPermissionRequest: approveAll,
    });

    const sessionId = this.session.id ?? this.session.sessionId ?? 'default-session';
    console.error(`[bridge] Session created: ${sessionId}`);

    return sessionId;
  }

  async sendPrompt(prompt: string): Promise<string> {
    if (!this.session) {
      throw new Error('No active session');
    }

    console.error(`[bridge] Sending prompt (${prompt.length} chars)`);

    // Subscribe to streaming events. Each returns an unsubscribe fn.
    const unsubs: Array<() => void> = [];
    let deltasSeen = 0;

    // Text deltas
    unsubs.push(
      this.session.on('assistant.message_delta', (event: any) => {
        const delta: string = event?.data?.deltaContent ?? '';
        if (delta) {
          deltasSeen++;
          writeStream({ event: 'text_delta', delta });
        }
      }),
    );

    // Fallback: some models (e.g. gpt-4o) emit only the final assistant.message
    // without per-token deltas. If no deltas fired, emit the full content as one delta.
    unsubs.push(
      this.session.on('assistant.message', (event: any) => {
        if (deltasSeen === 0) {
          const content: string = event?.data?.content ?? '';
          if (content) {
            writeStream({ event: 'text_delta', delta: content });
          }
        }
        deltasSeen = 0;
      }),
    );

    // Reasoning deltas
    unsubs.push(
      this.session.on('assistant.reasoning_delta', (event: any) => {
        const delta: string = event?.data?.deltaContent ?? '';
        if (delta) {
          writeStream({ event: 'reasoning_delta', delta });
        }
      }),
    );

    // Tool execution start
    unsubs.push(
      this.session.on('tool.execution_start', (event: any) => {
        const d = event?.data ?? {};
        writeStream({
          event: 'tool_started',
          call_id: d.toolCallId ?? '',
          name: d.toolName ?? '',
          args: d.arguments ?? {},
        });
      }),
    );

    // Tool execution complete
    unsubs.push(
      this.session.on('tool.execution_complete', (event: any) => {
        const d = event?.data ?? {};
        const success: boolean = d.success ?? true;
        const output: string =
          d.result?.detailedContent ?? d.result?.content ?? '';
        // error info: look for nested error object
        const errMsg: string | undefined = !success
          ? (d.error?.message ?? d.error?.text ?? 'Tool failed')
          : undefined;
        writeStream({
          event: 'tool_completed',
          call_id: d.toolCallId ?? '',
          output,
          ...(errMsg !== undefined ? { error: errMsg } : {}),
        });
      }),
    );

    // Session error
    unsubs.push(
      this.session.on('session.error', (event: any) => {
        const msg: string = event?.data?.message ?? 'Unknown Copilot error';
        console.error(`[bridge] Session error: ${msg}`);
        writeStream({ event: 'info', message: `Copilot error: ${msg}` });
      }),
    );

    try {
      // sendAndWait blocks until session.idle, streaming events fire via the handlers above.
      const response = await this.session.sendAndWait({ prompt }, 120_000);
      const content: string =
        response?.data?.content ?? '[No response from Copilot]';
      console.error('[bridge] Turn complete');
      return content;
    } finally {
      // Always unsubscribe so handlers from this turn don't leak into the next.
      unsubs.forEach((fn) => fn());
    }
  }

  async interrupt(): Promise<void> {
    if (!this.session) return;
    console.error('[bridge] Interrupting session...');
    try {
      await this.session.interrupt();
    } catch {
      // interrupt may throw if not in-flight; ignore
    }
  }

  async stop(): Promise<void> {
    if (this.session) {
      try {
        await this.session.disconnect();
      } catch {
        // ignore
      }
      this.session = null;
    }
    if (this.client) {
      try {
        await this.client.stop();
      } catch {
        // ignore
      }
      this.client = null;
    }
  }
}

// Main entry point
async function main(): Promise<void> {
  const bridge = new CopilotBridge();

  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  try {
    await bridge.start();

    // Send ready signal
    process.stdout.write(JSON.stringify({ type: 'ready' }) + '\n');
    console.error('[bridge] Ready for commands');

    // Process incoming messages
    for await (const line of rl) {
      try {
        const msg = JSON.parse(line) as ZenUiMessage;
        console.error(`[bridge] Received: ${msg.type}`);

        switch (msg.type) {
          case 'create_session': {
            const cwd = (msg.cwd as string) ?? process.cwd();
            const model = msg.model as string | undefined;
            const sessionId = await bridge.createSession(cwd, model);
            process.stdout.write(
              JSON.stringify({ type: 'session_created', session_id: sessionId }) + '\n',
            );
            break;
          }

          case 'send_prompt': {
            const prompt = msg.prompt as string;
            try {
              const output = await bridge.sendPrompt(prompt);
              process.stdout.write(
                JSON.stringify({ type: 'response', output }) + '\n',
              );
            } catch (err) {
              process.stdout.write(
                JSON.stringify({
                  type: 'error',
                  error: err instanceof Error ? err.message : String(err),
                }) + '\n',
              );
            }
            break;
          }

          case 'interrupt': {
            await bridge.interrupt();
            process.stdout.write(
              JSON.stringify({ type: 'interrupted' }) + '\n',
            );
            break;
          }

          case 'shutdown': {
            console.error('[bridge] Shutdown requested');
            await bridge.stop();
            process.exit(0);
          }

          default:
            console.error(`[bridge] Unknown message type: ${msg.type}`);
            process.stdout.write(
              JSON.stringify({
                type: 'error',
                error: `Unknown type: ${msg.type}`,
              }) + '\n',
            );
        }
      } catch (err) {
        console.error('[bridge] Error processing message:', err);
        process.stdout.write(
          JSON.stringify({
            type: 'error',
            error: err instanceof Error ? err.message : String(err),
          }) + '\n',
        );
      }
    }
  } finally {
    await bridge.stop();
  }
}

main().catch((err) => {
  console.error('[bridge] Fatal error:', err);
  process.exit(1);
});
