#!/usr/bin/env node
/**
 * GitHub Copilot SDK Bridge for ZenUI
 * 
 * This bridge spawns the GitHub Copilot CLI in ACP (Agent Client Protocol) server mode
 * and translates between the ZenUI protocol and Copilot's JSON-RPC protocol.
 */

import { spawn } from 'child_process';
import { readFileSync } from 'fs';
import { createInterface } from 'readline';

// Protocol types
interface JsonRpcRequest {
  jsonrpc: '2.0';
  id: number | string;
  method: string;
  params?: unknown;
}

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number | string;
  result?: unknown;
  error?: {
    code: number;
    message: string;
    data?: unknown;
  };
}

// ZenUI protocol types
interface ZenUiMessage {
  type: string;
  [key: string]: unknown;
}

class CopilotBridge {
  private copilotProcess: ReturnType<typeof spawn> | null = null;
  private requestId = 1;
  private pendingRequests = new Map<number | string, (response: JsonRpcResponse) => void>();
  private sessionId: string | null = null;

  async start(): Promise<void> {
    // Spawn Copilot CLI in ACP server mode
    this.copilotProcess = spawn('copilot', ['--acp', '--stdio'], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: {
        ...process.env,
        // Ensure Copilot CLI uses the correct authentication
        COPILOT_ALLOW_ALL: 'true',
      },
    });

    // Handle stdout (JSON-RPC responses)
    const rl = createInterface({
      input: this.copilotProcess.stdout!,
      crlfDelay: Infinity,
    });

    rl.on('line', (line) => {
      this.handleLine(line);
    });

    // Handle stderr (logging)
    this.copilotProcess.stderr!.on('data', (data) => {
      const msg = data.toString().trim();
      if (msg) {
        console.error(`[copilot-stderr] ${msg}`);
      }
    });

    // Handle process exit
    this.copilotProcess.on('exit', (code) => {
      console.error(`[bridge] Copilot CLI exited with code ${code}`);
      process.exit(code ?? 1);
    });

    // Initialize the connection
    await this.initialize();
  }

  private async handleLine(line: string): Promise<void> {
    try {
      const msg = JSON.parse(line) as JsonRpcResponse | JsonRpcRequest;
      
      if ('id' in msg && 'result' in msg) {
        // This is a response
        const handler = this.pendingRequests.get(msg.id);
        if (handler) {
          handler(msg);
          this.pendingRequests.delete(msg.id);
        }
      } else if ('method' in msg) {
        // This is a request from Copilot (server -> client)
        await this.handleServerRequest(msg as JsonRpcRequest);
      }
    } catch (err) {
      console.error(`[bridge] Failed to parse line: ${line}`, err);
    }
  }

  private async handleServerRequest(req: JsonRpcRequest): Promise<void> {
    // Handle server requests (like tool confirmations)
    // For now, auto-accept all tool requests
    const response: JsonRpcResponse = {
      jsonrpc: '2.0',
      id: req.id,
      result: { accepted: true },
    };
    this.send(response);
  }

  private async initialize(): Promise<void> {
    // Send initialize request per JSON-RPC spec
    const initRequest: JsonRpcRequest = {
      jsonrpc: '2.0',
      id: this.requestId++,
      method: 'initialize',
      params: {
        protocolVersion: '2024-11-05',
        capabilities: {},
        clientInfo: {
          name: 'zenui-copilot-bridge',
          version: '0.1.0',
        },
      },
    };

    const response = await this.request(initRequest);
    if (response.error) {
      throw new Error(`Initialize failed: ${response.error.message}`);
    }
    console.error('[bridge] Connected to Copilot CLI');
  }

  private request(req: JsonRpcRequest): Promise<JsonRpcResponse> {
    return new Promise((resolve) => {
      this.pendingRequests.set(req.id, resolve);
      this.send(req);
    });
  }

  private send(msg: JsonRpcRequest | JsonRpcResponse): void {
    const line = JSON.stringify(msg);
    this.copilotProcess!.stdin!.write(line + '\n');
  }

  async createSession(cwd: string): Promise<string> {
    const req: JsonRpcRequest = {
      jsonrpc: '2.0',
      id: this.requestId++,
      method: 'session/create',
      params: {
        clientName: 'zenui',
        cwd,
      },
    };

    const response = await this.request(req);
    if (response.error) {
      throw new Error(`Create session failed: ${response.error.message}`);
    }

    const result = response.result as { session?: { id: string } };
    this.sessionId = result.session?.id ?? null;
    if (!this.sessionId) {
      throw new Error('No session ID returned');
    }
    return this.sessionId;
  }

  async sendPrompt(prompt: string): Promise<string> {
    if (!this.sessionId) {
      throw new Error('No active session');
    }

    const req: JsonRpcRequest = {
      jsonrpc: '2.0',
      id: this.requestId++,
      method: 'session/send',
      params: {
        sessionId: this.sessionId,
        prompt,
        streaming: false,
      },
    };

    const response = await this.request(req);
    if (response.error) {
      throw new Error(`Send failed: ${response.error.message}`);
    }

    const result = response.result as { response?: { content?: string } };
    return result.response?.content ?? '';
  }

  async interrupt(): Promise<void> {
    if (!this.sessionId) return;

    const req: JsonRpcRequest = {
      jsonrpc: '2.0',
      id: this.requestId++,
      method: 'session/interrupt',
      params: {
        sessionId: this.sessionId,
      },
    };

    await this.request(req);
  }
}

// Main entry point
async function main(): Promise<void> {
  const bridge = new CopilotBridge();
  
  // Read and process ZenUI messages from stdin
  const rl = createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
  });

  // Start the bridge
  await bridge.start();

  // Send ready signal
  console.log(JSON.stringify({ type: 'ready' }));

  // Process incoming messages
  for await (const line of rl) {
    try {
      const msg = JSON.parse(line) as ZenUiMessage;
      
      switch (msg.type) {
        case 'create_session': {
          const cwd = msg.cwd as string;
          const sessionId = await bridge.createSession(cwd);
          console.log(JSON.stringify({
            type: 'session_created',
            sessionId,
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
      }
    } catch (err) {
      console.log(JSON.stringify({
        type: 'error',
        error: err instanceof Error ? err.message : String(err),
      }));
    }
  }
}

main().catch((err) => {
  console.error('[bridge] Fatal error:', err);
  process.exit(1);
});
