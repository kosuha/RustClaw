import fs from 'fs';
import path from 'path';
import readline from 'readline';
import { spawn, spawnSync } from 'child_process';

interface ContainerInput {
  prompt: string;
  sessionId?: string;
  groupFolder: string;
  chatJid: string;
  isMain: boolean;
  isScheduledTask?: boolean;
}

interface ContainerOutput {
  status: 'success' | 'error';
  result: string | null;
  newSessionId?: string;
  error?: string;
}

type JsonObject = Record<string, unknown>;

const OUTPUT_START_MARKER = '---NANOCLAW_OUTPUT_START---';
const OUTPUT_END_MARKER = '---NANOCLAW_OUTPUT_END---';

const IPC_INPUT_DIR = '/workspace/ipc/input';
const IPC_INPUT_CLOSE_SENTINEL = path.join(IPC_INPUT_DIR, '_close');
const IPC_POLL_MS = 500;

const MCP_SERVER_NAME = 'nanoclaw';
const MCP_SERVER_PATH = '/app/dist/ipc-mcp-stdio.js';

function writeOutput(output: ContainerOutput): void {
  console.log(OUTPUT_START_MARKER);
  console.log(JSON.stringify(output));
  console.log(OUTPUT_END_MARKER);
}

function log(message: string): void {
  console.error(`[agent-runner] ${message}`);
}

function rpcLog(scope: 'request' | 'event' | 'error', message: string): void {
  console.error(`[codex-rpc:${scope}] ${message}`);
}

async function readStdin(): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (chunk) => {
      data += chunk;
    });
    process.stdin.on('end', () => resolve(data));
    process.stdin.on('error', reject);
  });
}

function shouldClose(): boolean {
  if (!fs.existsSync(IPC_INPUT_CLOSE_SENTINEL)) {
    return false;
  }
  try {
    fs.unlinkSync(IPC_INPUT_CLOSE_SENTINEL);
  } catch {
    // no-op
  }
  return true;
}

function drainIpcInput(): string[] {
  try {
    fs.mkdirSync(IPC_INPUT_DIR, { recursive: true });
    const files = fs
      .readdirSync(IPC_INPUT_DIR)
      .filter((file) => file.endsWith('.json'))
      .sort();

    const messages: string[] = [];
    for (const file of files) {
      const filePath = path.join(IPC_INPUT_DIR, file);
      try {
        const payload = JSON.parse(fs.readFileSync(filePath, 'utf8')) as JsonObject;
        fs.unlinkSync(filePath);
        if (payload.type === 'message' && typeof payload.text === 'string' && payload.text.trim()) {
          messages.push(payload.text);
        }
      } catch (error) {
        log(`failed to parse IPC input file ${file}: ${toErrorMessage(error)}`);
        try {
          fs.unlinkSync(filePath);
        } catch {
          // no-op
        }
      }
    }
    return messages;
  } catch (error) {
    log(`failed to drain IPC input: ${toErrorMessage(error)}`);
    return [];
  }
}

function waitForIpcMessage(): Promise<string | null> {
  return new Promise((resolve) => {
    const poll = () => {
      if (shouldClose()) {
        resolve(null);
        return;
      }
      const messages = drainIpcInput();
      if (messages.length > 0) {
        resolve(messages.join('\n'));
        return;
      }
      setTimeout(poll, IPC_POLL_MS);
    };
    poll();
  });
}

function toErrorMessage(value: unknown): string {
  return value instanceof Error ? value.message : String(value);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function extractThreadId(value: unknown): string | null {
  if (!value || typeof value !== 'object') {
    return null;
  }
  const record = value as JsonObject;
  if (typeof record.threadId === 'string') {
    return record.threadId;
  }
  if (record.thread && typeof record.thread === 'object') {
    const thread = record.thread as JsonObject;
    if (typeof thread.id === 'string') {
      return thread.id;
    }
  }
  if (typeof record.id === 'string') {
    return record.id;
  }
  return null;
}

function extractTurnId(value: unknown): string | null {
  if (!value || typeof value !== 'object') {
    return null;
  }
  const record = value as JsonObject;
  if (typeof record.turnId === 'string') {
    return record.turnId;
  }
  if (record.turn && typeof record.turn === 'object') {
    const turn = record.turn as JsonObject;
    if (typeof turn.id === 'string') {
      return turn.id;
    }
  }
  if (typeof record.id === 'string') {
    return record.id;
  }
  return null;
}

function extractAgentMessageText(params: unknown): string | null {
  if (!params || typeof params !== 'object') {
    return null;
  }
  const record = params as JsonObject;
  const item = record.item;
  if (!item || typeof item !== 'object') {
    return null;
  }
  const itemRecord = item as JsonObject;
  const itemTypeRaw =
    typeof itemRecord.type === 'string'
      ? itemRecord.type
      : typeof itemRecord.kind === 'string'
        ? itemRecord.kind
        : '';
  const itemType = itemTypeRaw.toLowerCase();
  if (
    itemType !== 'assistantmessage' &&
    itemType !== 'agentmessage' &&
    itemType !== 'assistant'
  ) {
    return null;
  }
  if (typeof itemRecord.text === 'string') {
    return itemRecord.text;
  }
  if (typeof itemRecord.message === 'string') {
    return itemRecord.message;
  }
  if (Array.isArray(itemRecord.content)) {
    const chunks = itemRecord.content
      .map((entry) => {
        if (typeof entry === 'string') {
          return entry;
        }
        if (!entry || typeof entry !== 'object') {
          return '';
        }
        const value = entry as JsonObject;
        if (typeof value.text === 'string') {
          return value.text;
        }
        if (typeof value.value === 'string') {
          return value.value;
        }
        return '';
      })
      .filter((text) => text.length > 0);
    if (chunks.length > 0) {
      return chunks.join('');
    }
  }
  return null;
}

function normalizeApprovalPolicy(raw: string): string {
  switch (raw.trim().toLowerCase()) {
    case 'onfailure':
    case 'on-failure':
      return 'on-failure';
    case 'onrequest':
    case 'on-request':
      return 'on-request';
    case 'untrusted':
      return 'untrusted';
    case 'never':
      return 'never';
    default:
      return raw;
  }
}

function normalizeSandbox(raw: string): string {
  switch (raw.trim().toLowerCase()) {
    case 'dangerfullaccess':
    case 'danger-full-access':
      return 'danger-full-access';
    case 'workspacewrite':
    case 'workspace-write':
      return 'workspace-write';
    case 'readonly':
    case 'read-only':
      return 'read-only';
    default:
      return raw;
  }
}

function normalizeReasoningEffort(raw: string | undefined): string {
  const value = raw?.trim().toLowerCase();
  if (!value) {
    return 'medium';
  }
  switch (value) {
    case 'none':
    case 'minimal':
    case 'low':
    case 'medium':
    case 'high':
    case 'xhigh':
      return value;
    default:
      log(`invalid CODEX_REASONING_EFFORT='${raw}', fallback to 'medium'`);
      return 'medium';
  }
}

function toSandboxPolicyType(sandbox: string): string {
  switch (sandbox.trim().toLowerCase()) {
    case 'danger-full-access':
      return 'dangerFullAccess';
    case 'workspace-write':
      return 'workspaceWrite';
    case 'read-only':
      return 'readOnly';
    default:
      return sandbox;
  }
}

function extractErrorMessage(params: unknown): string {
  if (!params || typeof params !== 'object') {
    return 'unknown turn error';
  }
  const record = params as JsonObject;
  const err = record.error;
  if (err && typeof err === 'object') {
    const errRecord = err as JsonObject;
    if (typeof errRecord.message === 'string' && errRecord.message.trim()) {
      return errRecord.message;
    }
  }
  if (typeof record.message === 'string' && record.message.trim()) {
    return record.message;
  }
  return JSON.stringify(params);
}

function shouldRetryTurnError(params: unknown): boolean {
  if (!params || typeof params !== 'object') {
    return false;
  }
  const record = params as JsonObject;
  return record.willRetry === true;
}

function normalizeInputSessionId(sessionId: string | undefined): string | undefined {
  if (!sessionId) {
    return undefined;
  }
  if (sessionId.startsWith('codex:')) {
    return sessionId.slice('codex:'.length);
  }
  return sessionId;
}

function formatAuthAwareError(error: unknown): string {
  const message = toErrorMessage(error);
  const lowered = message.toLowerCase();
  if (
    lowered.includes('auth') ||
    lowered.includes('unauthorized') ||
    lowered.includes('forbidden') ||
    lowered.includes('login')
  ) {
    return 'Codex authentication required. Run `codex login` on the host and retry.';
  }
  return message;
}

function runCodex(args: string[], allowFailure = false): void {
  const result = spawnSync('codex', args, {
    encoding: 'utf8',
    env: process.env,
  });
  if (allowFailure) {
    return;
  }
  if (result.status === 0) {
    return;
  }
  throw new Error(
    `codex ${args.join(' ')} failed (status=${result.status}): ${result.stderr || result.stdout}`,
  );
}

function configureMcpServer(input: ContainerInput): void {
  runCodex(['mcp', 'remove', MCP_SERVER_NAME], true);
  runCodex([
    'mcp',
    'add',
    '--env',
    `NANOCLAW_CHAT_JID=${input.chatJid}`,
    '--env',
    `NANOCLAW_GROUP_FOLDER=${input.groupFolder}`,
    '--env',
    `NANOCLAW_IS_MAIN=${input.isMain ? '1' : '0'}`,
    MCP_SERVER_NAME,
    'node',
    MCP_SERVER_PATH,
  ]);
}

type NotificationHandler = (method: string, params: unknown) => void;

class CodexAppServerClient {
  private readonly child = spawn('codex', this.args(), {
    stdio: ['pipe', 'pipe', 'pipe'],
    env: process.env,
  });
  private readonly pending = new Map<
    number,
    {
      method: string;
      resolve: (value: unknown) => void;
      reject: (error: Error) => void;
      timeout: NodeJS.Timeout;
    }
  >();
  private readonly handlers = new Set<NotificationHandler>();
  private nextId = 1;
  private closed = false;

  constructor() {
    const stdoutReader = readline.createInterface({
      input: this.child.stdout,
      crlfDelay: Infinity,
    });
    stdoutReader.on('line', (line) => this.onStdoutLine(line));

    const stderrReader = readline.createInterface({
      input: this.child.stderr,
      crlfDelay: Infinity,
    });
    stderrReader.on('line', (line) => rpcLog('error', line));

    this.child.on('exit', (code, signal) => {
      this.closed = true;
      for (const [id, pending] of this.pending) {
        clearTimeout(pending.timeout);
        pending.reject(
          new Error(
            `codex app-server exited before response for request ${pending.method} (id=${id}, code=${code}, signal=${signal})`,
          ),
        );
      }
      this.pending.clear();
    });
  }

  onNotification(handler: NotificationHandler): () => void {
    this.handlers.add(handler);
    return () => this.handlers.delete(handler);
  }

  async initialize(): Promise<void> {
    await this.request('initialize', {
      clientInfo: {
        name: 'rust-claw-codex-agent-runner',
        version: '1.0.0',
      },
    });
    this.notify('initialized', {});
  }

  request(method: string, params: unknown, timeoutMs = 120000): Promise<unknown> {
    if (this.closed) {
      return Promise.reject(new Error('codex app-server already closed'));
    }

    const id = this.nextId++;
    const requestPayload = {
      jsonrpc: '2.0',
      id,
      method,
      params,
    };
    rpcLog('request', `${method} id=${id}`);

    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timeout waiting for response to ${method} (id=${id})`));
      }, timeoutMs);

      this.pending.set(id, {
        method,
        resolve,
        reject,
        timeout,
      });

      this.child.stdin.write(`${JSON.stringify(requestPayload)}\n`);
    });
  }

  notify(method: string, params: unknown): void {
    if (this.closed) {
      return;
    }
    const payload = {
      jsonrpc: '2.0',
      method,
      params,
    };
    this.child.stdin.write(`${JSON.stringify(payload)}\n`);
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.child.kill('SIGTERM');
  }

  private args(): string[] {
    const args = ['app-server'];
    const model = process.env.CODEX_MODEL?.trim();
    if (model) {
      args.push('-c', `model=${JSON.stringify(model)}`);
    }
    return args;
  }

  private onStdoutLine(line: string): void {
    let parsed: JsonObject;
    try {
      parsed = JSON.parse(line) as JsonObject;
    } catch {
      return;
    }

    if (typeof parsed.id === 'number') {
      const pending = this.pending.get(parsed.id);
      if (!pending) {
        return;
      }
      this.pending.delete(parsed.id);
      clearTimeout(pending.timeout);

      if ('error' in parsed && parsed.error) {
        pending.reject(new Error(JSON.stringify(parsed.error)));
      } else {
        pending.resolve(parsed.result);
      }
      return;
    }

    if (typeof parsed.method === 'string') {
      rpcLog('event', parsed.method);
      for (const handler of this.handlers) {
        handler(parsed.method, parsed.params);
      }
    }
  }
}

async function ensureThread(
  client: CodexAppServerClient,
  requestedSessionId: string | undefined,
): Promise<string> {
  const approvalPolicy = normalizeApprovalPolicy(
    process.env.CODEX_APPROVAL_POLICY?.trim() || 'never',
  );
  const sandbox = normalizeSandbox(
    process.env.CODEX_SANDBOX_MODE?.trim() || 'danger-full-access',
  );
  const resumeThreadId = normalizeInputSessionId(requestedSessionId);
  if (resumeThreadId) {
    try {
      const resumed = await client.request('thread/resume', { threadId: resumeThreadId });
      const resumedId = extractThreadId(resumed);
      return resumedId ?? resumeThreadId;
    } catch (error) {
      log(`thread resume failed, creating new thread: ${toErrorMessage(error)}`);
    }
  }

  const started = await client.request('thread/start', {
    cwd: '/workspace/group',
    approvalPolicy,
    sandbox,
  });
  const threadId = extractThreadId(started);
  if (!threadId) {
    throw new Error(`thread/start did not return threadId: ${JSON.stringify(started)}`);
  }
  return threadId;
}

async function runTurn(
  client: CodexAppServerClient,
  threadId: string,
  prompt: string,
): Promise<{ closedDuringTurn: boolean }> {
  const approvalPolicy = normalizeApprovalPolicy(
    process.env.CODEX_APPROVAL_POLICY?.trim() || 'never',
  );
  const sandbox = normalizeSandbox(
    process.env.CODEX_SANDBOX_MODE?.trim() || 'danger-full-access',
  );
  const sandboxPolicy = {
    type: toSandboxPolicyType(sandbox),
  };
  const enableSearchRaw = process.env.CODEX_ENABLE_SEARCH?.trim().toLowerCase();
  const enableSearch = enableSearchRaw ? !['0', 'false', 'no', 'off'].includes(enableSearchRaw) : true;
  const reasoningEffort = normalizeReasoningEffort(process.env.CODEX_REASONING_EFFORT);

  let turnId = '';
  let closedDuringTurn = false;
  let turnDoneResolve: (() => void) | null = null;
  let turnDoneReject: ((error: Error) => void) | null = null;

  const turnDone = new Promise<void>((resolve, reject) => {
    turnDoneResolve = resolve;
    turnDoneReject = reject;
  });

  const unsubscribe = client.onNotification((method, params) => {
    if (method === 'item/completed') {
      const text = extractAgentMessageText(params);
      if (text && text.trim()) {
        writeOutput({
          status: 'success',
          result: text,
          newSessionId: threadId,
        });
      }
      return;
    }

    if (method === 'error') {
      if (shouldRetryTurnError(params)) {
        return;
      }
      turnDoneReject?.(new Error(extractErrorMessage(params)));
      return;
    }

    if (method === 'turn/failed') {
      turnDoneReject?.(new Error(extractErrorMessage(params)));
      return;
    }

    if (method === 'turn/completed') {
      turnDoneResolve?.();
      return;
    }

    if (method === 'turn/interrupted') {
      turnDoneResolve?.();
    }
  });

  const started = await client.request('turn/start', {
    threadId,
    input: [{ type: 'text', text: prompt }],
    approvalPolicy,
    sandboxPolicy,
    search: enableSearch,
    effort: reasoningEffort,
  });
  const extractedTurnId = extractTurnId(started);
  if (!extractedTurnId) {
    unsubscribe();
    throw new Error(`turn/start did not return turnId: ${JSON.stringify(started)}`);
  }
  turnId = extractedTurnId;

  let monitorActive = true;
  const monitor = (async () => {
    while (monitorActive) {
      if (shouldClose()) {
        closedDuringTurn = true;
        try {
          await client.request('turn/interrupt', { threadId, turnId }, 30000);
        } catch (error) {
          log(`turn/interrupt failed: ${toErrorMessage(error)}`);
        }
        break;
      }

      const messages = drainIpcInput();
      for (const message of messages) {
        try {
          await client.request(
            'turn/steer',
            {
              threadId,
              expectedTurnId: turnId,
              input: [{ type: 'text', text: message }],
            },
            30000,
          );
        } catch (error) {
          log(`turn/steer failed: ${toErrorMessage(error)}`);
        }
      }

      await sleep(IPC_POLL_MS);
    }
  })();

  try {
    await turnDone;
  } finally {
    monitorActive = false;
    await monitor;
    unsubscribe();
  }

  return { closedDuringTurn };
}

async function main(): Promise<void> {
  let client: CodexAppServerClient | null = null;
  try {
    const stdin = await readStdin();
    const input = JSON.parse(stdin) as ContainerInput;
    try {
      fs.unlinkSync('/tmp/input.json');
    } catch {
      // no-op
    }

    fs.mkdirSync(IPC_INPUT_DIR, { recursive: true });
    try {
      fs.unlinkSync(IPC_INPUT_CLOSE_SENTINEL);
    } catch {
      // no-op
    }

    configureMcpServer(input);

    client = new CodexAppServerClient();
    await client.initialize();
    const threadId = await ensureThread(client, input.sessionId);

    let prompt = input.prompt;
    if (input.isScheduledTask) {
      prompt = `[SCHEDULED TASK]\n\n${prompt}`;
    }
    const pending = drainIpcInput();
    if (pending.length > 0) {
      prompt = `${prompt}\n${pending.join('\n')}`;
    }

    while (true) {
      const turnResult = await runTurn(client, threadId, prompt);

      if (turnResult.closedDuringTurn) {
        break;
      }

      writeOutput({
        status: 'success',
        result: null,
        newSessionId: threadId,
      });

      // Scheduled tasks are one-shot runs; do not keep container open for follow-up steer.
      if (input.isScheduledTask) {
        break;
      }

      const nextInput = await waitForIpcMessage();
      if (nextInput === null) {
        break;
      }
      prompt = nextInput;
    }
  } catch (error) {
    writeOutput({
      status: 'error',
      result: null,
      error: formatAuthAwareError(error),
    });
    process.exitCode = 1;
  } finally {
    client?.close();
  }
}

main();
