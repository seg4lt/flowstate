#!/usr/bin/env node
/**
 * GitHub Copilot SDK Bridge for ZenUI
 * 
 * This bridge uses the official @github/copilot-sdk to communicate
 * with the GitHub Copilot CLI.
 */

import { CopilotClient, approveAll } from '@github/copilot-sdk';
import { createInterface } from 'readline';

// ZenUI protocol types
interface ZenUiMessage {
  type: string;
  [key: string]: unknown;
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

  async createSession(cwd: string): Promise<string> {
    if (!this.client) {
      throw new Error('Client not started');
    }

    console.error(`[bridge] Creating session in: ${cwd}`);
    
    this.session = await this.client.createSession({
      model: 'gpt-5',
      onPermissionRequest: approveAll, // Required: handle tool permissions
    });

    // Get session ID from session object
    const sessionId = this.session.id || 'default-session';
    console.error(`[bridge] Session created: ${sessionId}`);
    
    return sessionId;
  }

  async sendPrompt(prompt: string): Promise<string> {
    if (!this.session) {
      throw new Error('No active session');
    }

    console.error(`[bridge] Sending prompt: ${prompt.slice(0, 50)}...`);
    
    return new Promise((resolve, reject) => {
      let output = '';
      let done = false;

      // Set timeout
      const timeout = setTimeout(() => {
        if (!done) {
          done = true;
          resolve(output || '[No response from Copilot]');
        }
      }, 60000); // 60 second timeout

      // Listen for messages using typed events
      this.session!.on('assistant.message', (event: { data?: { content?: string } }) => {
        if (event.data?.content) {
          output += event.data.content;
        }
      });

      // Listen for completion
      this.session!.on('session.idle', () => {
        if (!done) {
          done = true;
          clearTimeout(timeout);
          resolve(output || '[Copilot returned empty response]');
        }
      });

      // Send the prompt
      this.session!.send({ prompt }).catch((err: Error) => {
        if (!done) {
          done = true;
          clearTimeout(timeout);
          reject(err);
        }
      });
    });
  }

  async interrupt(): Promise<void> {
    if (!this.session) return;
    console.error('[bridge] Interrupting session...');
    await this.session.interrupt();
  }

  async stop(): Promise<void> {
    if (this.session) {
      await this.session.disconnect();
      this.session = null;
    }
    if (this.client) {
      await this.client.stop();
      this.client = null;
    }
  }
}

// Main entry point
async function main(): Promise<void> {
  const bridge = new CopilotBridge();
  
  // Read ZenUI messages from stdin
  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  try {
    // Start the bridge
    await bridge.start();

    // Send ready signal
    console.log(JSON.stringify({ type: 'ready' }));
    console.error('[bridge] Ready for commands');

    // Process incoming messages
    for await (const line of rl) {
      try {
        const msg = JSON.parse(line) as ZenUiMessage;
        console.error(`[bridge] Received: ${msg.type}`);
        
        switch (msg.type) {
          case 'create_session': {
            const cwd = msg.cwd as string;
            const sessionId = await bridge.createSession(cwd);
            console.log(JSON.stringify({
              type: 'session_created',
              session_id: sessionId,
            }));
            break;
          }

          case 'send_prompt': {
            const prompt = msg.prompt as string;
            const output = await bridge.sendPrompt(prompt);
            console.log(JSON.stringify({
              type: 'response',
              output,
            }));
            break;
          }

          case 'interrupt': {
            await bridge.interrupt();
            console.log(JSON.stringify({
              type: 'interrupted',
            }));
            break;
          }

          default:
            console.error(`[bridge] Unknown message type: ${msg.type}`);
            console.log(JSON.stringify({
              type: 'error',
              error: `Unknown type: ${msg.type}`,
            }));
        }
      } catch (err) {
        console.error('[bridge] Error processing message:', err);
        console.log(JSON.stringify({
          type: 'error',
          error: err instanceof Error ? err.message : String(err),
        }));
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
