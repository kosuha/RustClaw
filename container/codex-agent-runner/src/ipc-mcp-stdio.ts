/**
 * Stdio MCP Server for NanoClaw
 * Standalone process that agent teams subagents can inherit.
 * Reads context from environment variables, writes IPC files for the host.
 */

import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
import fs from 'fs';
import path from 'path';
import { randomUUID } from 'crypto';
import { CronExpressionParser } from 'cron-parser';

const IPC_DIR = '/workspace/ipc';
const MESSAGES_DIR = path.join(IPC_DIR, 'messages');
const TASKS_DIR = path.join(IPC_DIR, 'tasks');
const SKILLS_FILE = path.join(IPC_DIR, 'current_skills.json');
const UPBIT_RESPONSES_DIR = path.join(IPC_DIR, 'responses');
const UPBIT_PROXY_TIMEOUT_MS = 45000;
const UPBIT_PROXY_POLL_MS = 120;

// Context from environment variables (set by the agent runner)
const chatJid = process.env.NANOCLAW_CHAT_JID!;
const groupFolder = process.env.NANOCLAW_GROUP_FOLDER!;
const isMain = process.env.NANOCLAW_IS_MAIN === '1';

type SkillSnapshotRow = {
  groupFolder?: string;
  name?: string;
  source?: string;
  enabled?: boolean;
  path?: string;
};

type SkillSnapshotPayload = {
  skills?: SkillSnapshotRow[];
  lastSync?: string;
};

type UpbitQueryPrimitive = string | number | boolean;
type UpbitQueryValue = UpbitQueryPrimitive | UpbitQueryPrimitive[] | undefined | null;
type UpbitQuery = Record<string, UpbitQueryValue>;
type UpbitRequestMethod = 'GET' | 'POST' | 'DELETE';

type UpbitRequestOptions = {
  pathname: string;
  method?: UpbitRequestMethod;
  query?: UpbitQuery;
  body?: UpbitQuery;
  requiresAuth: boolean;
  rateLimitGroupHint?: string;
  maxRetries?: number;
  intent?: UpbitProxyIntent;
};

type UpbitProxyIntent = {
  confirmRealOrder?: boolean;
};

type UpbitProxyResponse = {
  requestId?: string;
  ok?: boolean;
  result?: unknown;
  error?: string;
};

type UpbitTickerRow = {
  market?: string;
  trade_price?: number | string;
  signed_change_rate?: number | string;
  acc_trade_price_24h?: number | string;
};

type UpbitBalanceRow = {
  currency?: string;
  balance?: string;
  locked?: string;
  avg_buy_price?: string;
  unit_currency?: string;
};

type UpbitMarketRow = {
  market?: string;
  korean_name?: string;
  english_name?: string;
  market_warning?: string;
};

type UpbitOrderbookUnit = {
  ask_price?: number | string;
  bid_price?: number | string;
  ask_size?: number | string;
  bid_size?: number | string;
};

type UpbitOrderbookRow = {
  market?: string;
  total_ask_size?: number | string;
  total_bid_size?: number | string;
  orderbook_units?: UpbitOrderbookUnit[];
};

type UpbitCandleRow = {
  market?: string;
  candle_date_time_utc?: string;
  candle_date_time_kst?: string;
  opening_price?: number | string;
  high_price?: number | string;
  low_price?: number | string;
  trade_price?: number | string;
  candle_acc_trade_price?: number | string;
  candle_acc_trade_volume?: number | string;
  unit?: number;
};

type UpbitTradeTickRow = {
  market?: string;
  trade_date_utc?: string;
  trade_time_utc?: string;
  timestamp?: number | string;
  trade_price?: number | string;
  trade_volume?: number | string;
  ask_bid?: string;
  sequential_id?: number | string;
};

type UpbitOrderRow = {
  uuid?: string;
  identifier?: string;
  market?: string;
  side?: string;
  ord_type?: string;
  state?: string;
  price?: string;
  volume?: string;
  remaining_volume?: string;
  executed_volume?: string;
  executed_funds?: string;
  created_at?: string;
};

type UpbitOrderSide = 'bid' | 'ask';
type UpbitOrderType = 'limit' | 'price' | 'market' | 'best';
type UpbitTimeInForce = 'ioc' | 'fok' | 'post_only';
type UpbitSmpType = 'cancel_maker' | 'cancel_taker' | 'reduce';

type UpbitCreateOrderPayload = {
  market: string;
  side: UpbitOrderSide;
  ord_type: UpbitOrderType;
  volume?: string;
  price?: string;
  time_in_force?: UpbitTimeInForce;
  smp_type?: UpbitSmpType;
  identifier?: string;
};

type UpbitOrderChancePayload = {
  bid_fee?: string;
  ask_fee?: string;
  market?: {
    id?: string;
    state?: string;
    bid_types?: string[];
    ask_types?: string[];
    bid?: {
      min_total?: string;
      currency?: string;
    };
    ask?: {
      min_total?: string;
      currency?: string;
    };
    max_total?: string;
  };
};

const UPBIT_DEFAULT_MAX_RETRIES = 3;

function writeIpcFile(dir: string, data: object): string {
  fs.mkdirSync(dir, { recursive: true });

  const filename = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}.json`;
  const filepath = path.join(dir, filename);

  // Atomic write: temp file then rename
  const tempPath = `${filepath}.tmp`;
  fs.writeFileSync(tempPath, JSON.stringify(data, null, 2));
  fs.renameSync(tempPath, filepath);

  return filename;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function requestSkillsRefresh(targetGroupFolder?: string): Promise<void> {
  const payload: Record<string, string> = {
    type: 'refresh_skills',
    timestamp: new Date().toISOString(),
  };

  if (isMain && targetGroupFolder) {
    payload.targetGroupFolder = targetGroupFolder;
  }

  writeIpcFile(TASKS_DIR, payload);
}

function readSkillSnapshot(): SkillSnapshotPayload | null {
  try {
    if (!fs.existsSync(SKILLS_FILE)) {
      return null;
    }
    const parsed = JSON.parse(fs.readFileSync(SKILLS_FILE, 'utf-8')) as SkillSnapshotPayload;
    if (!parsed || !Array.isArray(parsed.skills)) {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}

async function waitForSkillSnapshot(maxWaitMs = 2500): Promise<SkillSnapshotPayload | null> {
  const startedAt = Date.now();
  while (Date.now() - startedAt < maxWaitMs) {
    const snapshot = readSkillSnapshot();
    if (snapshot) {
      return snapshot;
    }
    await sleep(250);
  }
  return readSkillSnapshot();
}

function listableSkills(
  snapshot: SkillSnapshotPayload,
  targetGroupFolder?: string,
): SkillSnapshotRow[] {
  const rows = Array.isArray(snapshot.skills) ? snapshot.skills : [];
  return rows.filter((row) => {
    const rowGroup = row.groupFolder || groupFolder;
    if (!isMain) {
      return rowGroup === groupFolder;
    }
    if (!targetGroupFolder) {
      return true;
    }
    return rowGroup === targetGroupFolder;
  });
}

function toErrorMessage(value: unknown): string {
  return value instanceof Error ? value.message : String(value);
}

function toUpbitQueryEntries(params: UpbitQuery = {}): Array<[string, string]> {
  const entries: Array<[string, string]> = [];
  for (const [key, rawValue] of Object.entries(params)) {
    if (typeof rawValue === 'undefined' || rawValue === null) {
      continue;
    }
    if (Array.isArray(rawValue)) {
      for (const item of rawValue) {
        entries.push([key, String(item)]);
      }
      continue;
    }
    entries.push([key, String(rawValue)]);
  }
  return entries;
}

function encodeUpbitQueryComponent(value: string): string {
  return encodeURIComponent(value).replace(/%5B/g, '[').replace(/%5D/g, ']');
}

function buildUpbitQueryString(params: UpbitQuery = {}, encode = false): string {
  const entries = toUpbitQueryEntries(params);
  if (entries.length === 0) {
    return '';
  }

  return entries
    .map(([key, value]) => {
      if (!encode) {
        return `${key}=${value}`;
      }
      return `${encodeUpbitQueryComponent(key)}=${encodeUpbitQueryComponent(value)}`;
    })
    .join('&');
}

function sanitizeUpbitPayload(payload?: UpbitQuery): UpbitQuery | undefined {
  if (!payload) {
    return undefined;
  }

  const sanitized = Object.entries(payload).reduce<UpbitQuery>((acc, [key, value]) => {
    if (typeof value === 'undefined' || value === null) {
      return acc;
    }
    acc[key] = value;
    return acc;
  }, {});

  return Object.keys(sanitized).length > 0 ? sanitized : undefined;
}

async function requestUpbit(options: UpbitRequestOptions): Promise<unknown> {
  const method = options.method || 'GET';
  const sanitizedQuery = sanitizeUpbitPayload(options.query);
  const sanitizedBody = sanitizeUpbitPayload(options.body);
  const requestId = `upbit_${randomUUID().replaceAll('-', '')}`;

  writeIpcFile(TASKS_DIR, {
    type: 'upbit_proxy_request',
    requestId,
    action: 'http_request',
    request: {
      pathname: options.pathname,
      method,
      query: sanitizedQuery,
      body: sanitizedBody,
      requiresAuth: options.requiresAuth,
      rateLimitGroupHint: options.rateLimitGroupHint,
      maxRetries: options.maxRetries ?? UPBIT_DEFAULT_MAX_RETRIES,
      intent: options.intent,
    },
    groupFolder,
    timestamp: new Date().toISOString(),
  });

  const response = await waitForUpbitProxyResponse(requestId);
  if (response.ok !== true) {
    throw new Error(response.error || 'Host Upbit broker returned an unknown error.');
  }

  return response.result ?? null;
}

function readUpbitProxyResponse(requestId: string): UpbitProxyResponse | null {
  const responseFile = path.join(UPBIT_RESPONSES_DIR, `${requestId}.json`);
  if (!fs.existsSync(responseFile)) {
    return null;
  }

  try {
    const parsed = JSON.parse(fs.readFileSync(responseFile, 'utf8')) as UpbitProxyResponse;
    try {
      fs.unlinkSync(responseFile);
    } catch {
      // no-op
    }
    return parsed;
  } catch (error) {
    try {
      fs.unlinkSync(responseFile);
    } catch {
      // no-op
    }
    throw new Error(`Failed to parse host Upbit broker response: ${toErrorMessage(error)}`);
  }
}

async function waitForUpbitProxyResponse(
  requestId: string,
  maxWaitMs = UPBIT_PROXY_TIMEOUT_MS,
): Promise<UpbitProxyResponse> {
  const startedAt = Date.now();
  while (Date.now() - startedAt <= maxWaitMs) {
    const response = readUpbitProxyResponse(requestId);
    if (response) {
      return response;
    }
    await sleep(UPBIT_PROXY_POLL_MS);
  }
  throw new Error(`Timed out waiting for Upbit host broker response (requestId=${requestId}).`);
}

async function requestUpbitPublic(pathname: string, query?: UpbitQuery): Promise<unknown> {
  return requestUpbit({
    pathname,
    method: 'GET',
    query,
    requiresAuth: false,
  });
}

async function requestUpbitPrivate(
  pathname: string,
  options: Omit<UpbitRequestOptions, 'pathname' | 'requiresAuth'> = {},
): Promise<unknown> {
  return requestUpbit({
    pathname,
    requiresAuth: true,
    ...options,
  });
}

function asNumber(value: unknown): number | null {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === 'string' && value.trim()) {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) {
      return parsed;
    }
  }
  return null;
}

function formatFixed(value: unknown, digits = 8): string {
  const numeric = asNumber(value);
  if (numeric === null) {
    return 'n/a';
  }
  return numeric.toFixed(digits);
}

function formatPercent(value: unknown): string {
  const numeric = asNumber(value);
  if (numeric === null) {
    return 'n/a';
  }
  return `${(numeric * 100).toFixed(2)}%`;
}

function formatUpbitJson(payload: unknown): string {
  const rendered = JSON.stringify(payload, null, 2);
  if (rendered.length <= 2500) {
    return rendered;
  }
  return `${rendered.slice(0, 2500)}\n... (truncated)`;
}

function normalizeMarkets(markets: string[]): string[] {
  return Array.from(
    new Set(markets.map((market) => market.trim().toUpperCase()).filter((market) => market)),
  );
}

function normalizeOptionalString(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed ? trimmed : undefined;
}

function buildOrderSelectorQuery(uuid?: string, identifier?: string): { query?: UpbitQuery; error?: string } {
  const normalizedUuid = normalizeOptionalString(uuid);
  const normalizedIdentifier = normalizeOptionalString(identifier);

  if (normalizedUuid && normalizedIdentifier) {
    return { error: 'Provide only one of uuid or identifier.' };
  }
  if (!normalizedUuid && !normalizedIdentifier) {
    return { error: 'Either uuid or identifier is required.' };
  }

  if (normalizedUuid) {
    return { query: { uuid: normalizedUuid } };
  }
  return { query: { identifier: normalizedIdentifier! } };
}

function formatUpbitOrderLine(row: UpbitOrderRow): string {
  const id = row.identifier || row.uuid || 'UNKNOWN_ID';
  const market = row.market || 'UNKNOWN';
  const side = row.side || 'unknown';
  const orderType = row.ord_type || 'unknown';
  const state = row.state || 'unknown';
  const remain = row.remaining_volume || row.volume || '-';
  const executed = row.executed_volume || '-';
  const price = row.price || '-';
  return `- ${id}: ${market} ${side}/${orderType}, state=${state}, remaining=${remain}, executed=${executed}, price=${price}`;
}

function formatUpbitCandleLine(row: UpbitCandleRow): string {
  const market = row.market || 'UNKNOWN';
  const time = row.candle_date_time_kst || row.candle_date_time_utc || 'unknown_time';
  const open = formatFixed(row.opening_price, 8);
  const high = formatFixed(row.high_price, 8);
  const low = formatFixed(row.low_price, 8);
  const close = formatFixed(row.trade_price, 8);
  const volume = formatFixed(row.candle_acc_trade_volume, 8);
  const unit = typeof row.unit === 'number' ? `, unit=${row.unit}m` : '';
  return `- ${market} @ ${time}: O=${open}, H=${high}, L=${low}, C=${close}, V=${volume}${unit}`;
}

function formatUpbitTradeLine(row: UpbitTradeTickRow): string {
  const market = row.market || 'UNKNOWN';
  const time = [row.trade_date_utc, row.trade_time_utc].filter(Boolean).join(' ') || 'unknown_time';
  const price = formatFixed(row.trade_price, 8);
  const volume = formatFixed(row.trade_volume, 8);
  const side = row.ask_bid || 'unknown';
  const seq = row.sequential_id ? `, seq=${String(row.sequential_id)}` : '';
  return `- ${market} @ ${time}: price=${price}, volume=${volume}, side=${side}${seq}`;
}

function parsePositiveNumber(value: string | undefined): number | null {
  if (!value) {
    return null;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return null;
  }
  return parsed;
}

function validateUpbitOrderPayload(payload: UpbitCreateOrderPayload): string[] {
  const errors: string[] = [];
  const volumeProvided = typeof payload.volume === 'string' && payload.volume.trim().length > 0;
  const priceProvided = typeof payload.price === 'string' && payload.price.trim().length > 0;
  const tif = payload.time_in_force;

  if (!payload.market.trim()) {
    errors.push('market is required.');
  }

  if (volumeProvided && parsePositiveNumber(payload.volume) === null) {
    errors.push('volume must be a positive number.');
  }
  if (priceProvided && parsePositiveNumber(payload.price) === null) {
    errors.push('price must be a positive number.');
  }

  if (tif && payload.ord_type === 'best' && !['ioc', 'fok'].includes(tif)) {
    errors.push('best order requires time_in_force to be ioc or fok.');
  }
  if (tif && payload.ord_type === 'limit' && !['ioc', 'fok', 'post_only'].includes(tif)) {
    errors.push('limit order time_in_force must be one of ioc, fok, post_only.');
  }
  if (tif && (payload.ord_type === 'price' || payload.ord_type === 'market')) {
    errors.push('time_in_force is only supported for limit or best orders.');
  }
  if (payload.time_in_force === 'post_only' && payload.smp_type) {
    errors.push('post_only cannot be used together with smp_type.');
  }

  if (payload.ord_type === 'limit') {
    if (!volumeProvided) {
      errors.push('limit order requires volume.');
    }
    if (!priceProvided) {
      errors.push('limit order requires price.');
    }
  } else if (payload.ord_type === 'price') {
    if (payload.side !== 'bid') {
      errors.push('ord_type=price is only valid for side=bid.');
    }
    if (!priceProvided) {
      errors.push('market buy (ord_type=price) requires price.');
    }
    if (volumeProvided) {
      errors.push('market buy (ord_type=price) must not include volume.');
    }
  } else if (payload.ord_type === 'market') {
    if (payload.side !== 'ask') {
      errors.push('ord_type=market is only valid for side=ask.');
    }
    if (!volumeProvided) {
      errors.push('market sell (ord_type=market) requires volume.');
    }
    if (priceProvided) {
      errors.push('market sell (ord_type=market) must not include price.');
    }
  } else if (payload.ord_type === 'best') {
    if (!tif) {
      errors.push('best order requires time_in_force (ioc or fok).');
    }
    if (payload.side === 'bid') {
      if (!priceProvided) {
        errors.push('best bid order requires price.');
      }
      if (volumeProvided) {
        errors.push('best bid order must not include volume.');
      }
    } else {
      if (!volumeProvided) {
        errors.push('best ask order requires volume.');
      }
      if (priceProvided) {
        errors.push('best ask order must not include price.');
      }
    }
  }

  return errors;
}

function buildUpbitOrderBody(payload: UpbitCreateOrderPayload): UpbitQuery {
  return {
    market: payload.market,
    side: payload.side,
    ord_type: payload.ord_type,
    volume: normalizeOptionalString(payload.volume),
    price: normalizeOptionalString(payload.price),
    time_in_force: payload.time_in_force,
    smp_type: payload.smp_type,
    identifier: normalizeOptionalString(payload.identifier),
  };
}

function upbitToolError(prefix: string, error: unknown): { content: Array<{ type: 'text'; text: string }>; isError: true } {
  return {
    content: [
      {
        type: 'text' as const,
        text: `${prefix}: ${toErrorMessage(error)}`,
      },
    ],
    isError: true,
  };
}

const server = new McpServer({
  name: 'nanoclaw',
  version: '1.0.0',
});

server.tool(
  'send_message',
  "Send a message to the user or group immediately while you're still running. Use this for progress updates or to send multiple messages. You can call this multiple times. Note: when running as a scheduled task, your final output is NOT sent to the user — use this tool if you need to communicate with the user or group.",
  {
    text: z.string().describe('The message text to send'),
    sender: z.string().optional().describe('Your role/identity name (e.g. "Researcher"). When set, messages appear from a dedicated bot in Telegram.'),
  },
  async (args) => {
    const data: Record<string, string | undefined> = {
      type: 'message',
      chatJid,
      text: args.text,
      sender: args.sender || undefined,
      groupFolder,
      timestamp: new Date().toISOString(),
    };

    writeIpcFile(MESSAGES_DIR, data);

    return { content: [{ type: 'text' as const, text: 'Message sent.' }] };
  },
);

server.tool(
  'upbit_get_markets',
  'Fetch all available markets from Upbit quotation API.',
  {
    is_details: z
      .boolean()
      .default(false)
      .describe('Include warning metadata when true.'),
    limit: z
      .number()
      .int()
      .min(1)
      .max(200)
      .default(50)
      .describe('Maximum number of rows to print.'),
  },
  async (args) => {
    try {
      const payload = await requestUpbitPublic('/v1/market/all', {
        is_details: args.is_details,
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected markets response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitMarketRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No markets returned.' }] };
      }

      const lines = rows
        .slice(0, args.limit)
        .map((row) => {
          const market = row.market || 'UNKNOWN';
          const koName = row.korean_name || '-';
          const enName = row.english_name || '-';
          const warning = row.market_warning ? `, warning=${row.market_warning}` : '';
          return `- ${market}: ko=${koName}, en=${enName}${warning}`;
        })
        .join('\n');
      const suffix = rows.length > args.limit ? `\n... (${rows.length - args.limit} more)` : '';

      return {
        content: [{ type: 'text' as const, text: `Upbit markets:\n${lines}${suffix}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit markets', error);
    }
  },
);

server.tool(
  'upbit_get_ticker',
  'Fetch ticker snapshots from Upbit quotation API for one or more markets (for example: KRW-BTC, KRW-ETH).',
  {
    markets: z
      .array(z.string())
      .min(1)
      .max(20)
      .describe('Upbit market codes (e.g. KRW-BTC, KRW-ETH).'),
  },
  async (args) => {
    const markets = normalizeMarkets(args.markets);
    if (markets.length === 0) {
      return {
        content: [{ type: 'text' as const, text: 'No valid markets were provided.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPublic('/v1/ticker', {
        markets: markets.join(','),
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected ticker response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitTickerRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No ticker data returned.' }] };
      }

      const lines = rows
        .map((row) => {
          const market = row.market || 'UNKNOWN';
          return `- ${market}: price=${formatFixed(row.trade_price, 2)}, change=${formatPercent(row.signed_change_rate)}, value_24h=${formatFixed(row.acc_trade_price_24h, 2)}`;
        })
        .join('\n');

      return {
        content: [{ type: 'text' as const, text: `Upbit ticker:\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit ticker', error);
    }
  },
);

server.tool(
  'upbit_get_orderbook',
  'Fetch orderbook snapshots from Upbit quotation API.',
  {
    markets: z
      .array(z.string())
      .min(1)
      .max(20)
      .describe('Upbit market codes (e.g. KRW-BTC, KRW-ETH).'),
  },
  async (args) => {
    const markets = normalizeMarkets(args.markets);
    if (markets.length === 0) {
      return {
        content: [{ type: 'text' as const, text: 'No valid markets were provided.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPublic('/v1/orderbook', {
        markets: markets.join(','),
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected orderbook response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitOrderbookRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No orderbook data returned.' }] };
      }

      const lines = rows
        .map((row) => {
          const market = row.market || 'UNKNOWN';
          const top = Array.isArray(row.orderbook_units) ? row.orderbook_units[0] : undefined;
          const bid = top ? formatFixed(top.bid_price, 2) : 'n/a';
          const ask = top ? formatFixed(top.ask_price, 2) : 'n/a';
          const bidSize = top ? formatFixed(top.bid_size, 6) : 'n/a';
          const askSize = top ? formatFixed(top.ask_size, 6) : 'n/a';
          return `- ${market}: bid=${bid} (${bidSize}), ask=${ask} (${askSize}), total_bid=${formatFixed(row.total_bid_size, 6)}, total_ask=${formatFixed(row.total_ask_size, 6)}`;
        })
        .join('\n');

      return {
        content: [{ type: 'text' as const, text: `Upbit orderbook:\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit orderbook', error);
    }
  },
);

server.tool(
  'upbit_get_recent_trades',
  'Fetch recent trades from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    count: z.number().int().min(1).max(500).default(20).describe('Number of trade rows to fetch.'),
    to: z
      .string()
      .optional()
      .describe('Optional UTC time cursor in HHmmss or HH:mm:ss format for the selected day.'),
    cursor: z.string().optional().describe('Optional pagination cursor(sequential_id).'),
    days_ago: z.number().int().min(1).max(7).optional().describe('Optional day offset(1..7, UTC-based).'),
  },
  async (args) => {
    const market = args.market.trim().toUpperCase();
    if (!market) {
      return {
        content: [{ type: 'text' as const, text: 'market is required.' }],
        isError: true,
      };
    }

    const query: UpbitQuery = {
      market,
      count: args.count,
      to: normalizeOptionalString(args.to),
      cursor: normalizeOptionalString(args.cursor),
      days_ago: args.days_ago,
    };

    try {
      const payload = await requestUpbitPublic('/v1/trades/ticks', query);
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected recent trades response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitTradeTickRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No recent trades returned.' }] };
      }

      const lines = rows.map((row) => formatUpbitTradeLine(row)).join('\n');
      return {
        content: [{ type: 'text' as const, text: `Upbit recent trades:\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit recent trades', error);
    }
  },
);

server.tool(
  'upbit_get_candles_minutes',
  'Fetch minute candles from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    unit: z
      .enum(['1', '3', '5', '10', '15', '30', '60', '240'])
      .default('15')
      .describe('Minute candle unit.'),
    count: z.number().int().min(1).max(200).default(30).describe('Number of candles to fetch.'),
    to: z.string().optional().describe('Optional candle end time (ISO 8601).'),
  },
  async (args) => {
    const market = args.market.trim().toUpperCase();
    if (!market) {
      return {
        content: [{ type: 'text' as const, text: 'market is required.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPublic(`/v1/candles/minutes/${args.unit}`, {
        market,
        count: args.count,
        to: normalizeOptionalString(args.to),
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected minute candles response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitCandleRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No minute candles returned.' }] };
      }

      const lines = rows.map((row) => formatUpbitCandleLine(row)).join('\n');
      return {
        content: [{ type: 'text' as const, text: `Upbit minute candles(${args.unit}m):\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit minute candles', error);
    }
  },
);

server.tool(
  'upbit_get_candles_days',
  'Fetch day candles from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    count: z.number().int().min(1).max(200).default(30).describe('Number of candles to fetch.'),
    to: z.string().optional().describe('Optional candle end time (ISO 8601).'),
    converting_price_unit: z
      .string()
      .optional()
      .describe('Optional converted close currency unit (for example KRW).'),
  },
  async (args) => {
    const market = args.market.trim().toUpperCase();
    if (!market) {
      return {
        content: [{ type: 'text' as const, text: 'market is required.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPublic('/v1/candles/days', {
        market,
        count: args.count,
        to: normalizeOptionalString(args.to),
        converting_price_unit: normalizeOptionalString(args.converting_price_unit)?.toUpperCase(),
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected day candles response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitCandleRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No day candles returned.' }] };
      }

      const lines = rows.map((row) => formatUpbitCandleLine(row)).join('\n');
      return {
        content: [{ type: 'text' as const, text: `Upbit day candles:\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit day candles', error);
    }
  },
);

async function runCandlePeriodTool(
  endpoint: '/v1/candles/weeks' | '/v1/candles/months' | '/v1/candles/years',
  label: string,
  args: { market: string; count: number; to?: string },
): Promise<{ content: Array<{ type: 'text'; text: string }>; isError?: true }> {
  const market = args.market.trim().toUpperCase();
  if (!market) {
    return {
      content: [{ type: 'text' as const, text: 'market is required.' }],
      isError: true,
    };
  }

  try {
    const payload = await requestUpbitPublic(endpoint, {
      market,
      count: args.count,
      to: normalizeOptionalString(args.to),
    });
    if (!Array.isArray(payload)) {
      return {
        content: [{ type: 'text' as const, text: `Unexpected ${label} candles response: ${formatUpbitJson(payload)}` }],
        isError: true,
      };
    }

    const rows = payload as UpbitCandleRow[];
    if (rows.length === 0) {
      return { content: [{ type: 'text' as const, text: `No ${label} candles returned.` }] };
    }

    const lines = rows.map((row) => formatUpbitCandleLine(row)).join('\n');
    return {
      content: [{ type: 'text' as const, text: `Upbit ${label} candles:\n${lines}` }],
    };
  } catch (error) {
    return upbitToolError(`Failed to fetch Upbit ${label} candles`, error);
  }
}

server.tool(
  'upbit_get_candles_weeks',
  'Fetch week candles from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    count: z.number().int().min(1).max(200).default(30).describe('Number of candles to fetch.'),
    to: z.string().optional().describe('Optional candle end time (ISO 8601).'),
  },
  async (args) => runCandlePeriodTool('/v1/candles/weeks', 'week', args),
);

server.tool(
  'upbit_get_candles_months',
  'Fetch month candles from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    count: z.number().int().min(1).max(200).default(30).describe('Number of candles to fetch.'),
    to: z.string().optional().describe('Optional candle end time (ISO 8601).'),
  },
  async (args) => runCandlePeriodTool('/v1/candles/months', 'month', args),
);

server.tool(
  'upbit_get_candles_years',
  'Fetch year candles from Upbit quotation API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    count: z.number().int().min(1).max(200).default(30).describe('Number of candles to fetch.'),
    to: z.string().optional().describe('Optional candle end time (ISO 8601).'),
  },
  async (args) => runCandlePeriodTool('/v1/candles/years', 'year', args),
);

server.tool(
  'upbit_get_balances',
  'Fetch authenticated account balances from Upbit exchange API. Requires host-side UPBIT_ACCESS_KEY and UPBIT_SECRET_KEY.',
  {
    hide_zero_balances: z
      .boolean()
      .default(true)
      .describe('If true, hide assets where both balance and locked are 0.'),
  },
  async (args) => {
    try {
      const payload = await requestUpbitPrivate('/v1/accounts', {
        method: 'GET',
        rateLimitGroupHint: 'default',
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected balance response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitBalanceRow[];
      const filtered = rows.filter((row) => {
        if (!args.hide_zero_balances) {
          return true;
        }
        const balance = asNumber(row.balance) || 0;
        const locked = asNumber(row.locked) || 0;
        return balance > 0 || locked > 0;
      });

      if (filtered.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No balances matched the filter.' }] };
      }

      const lines = filtered
        .map((row) => {
          const currency = row.currency || 'UNKNOWN';
          const unit = row.unit_currency || '';
          const avgBuyPrice = formatFixed(row.avg_buy_price, 8);
          return `- ${currency}: balance=${formatFixed(row.balance, 8)}, locked=${formatFixed(row.locked, 8)}, avg_buy_price=${avgBuyPrice}${unit ? ` ${unit}` : ''}`;
        })
        .join('\n');

      return {
        content: [{ type: 'text' as const, text: `Upbit balances:\n${lines}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit balances', error);
    }
  },
);

server.tool(
  'upbit_get_open_orders',
  'Fetch authenticated open orders from Upbit exchange API.',
  {
    market: z.string().optional().describe('Optional market code filter (e.g. KRW-BTC).'),
    state: z
      .enum(['wait', 'watch'])
      .optional()
      .describe('Optional single state filter. Cannot be used with states.'),
    states: z
      .array(z.enum(['wait', 'watch']))
      .min(1)
      .max(2)
      .optional()
      .describe('Optional multi-state filter. Cannot be used with state.'),
    page: z.number().int().min(1).default(1).describe('Pagination page number.'),
    limit: z.number().int().min(1).max(100).default(100).describe('Maximum rows to print.'),
    order_by: z.enum(['asc', 'desc']).default('desc').describe('Sort direction by creation time.'),
  },
  async (args) => {
    if (args.state && args.states && args.states.length > 0) {
      return {
        content: [{ type: 'text' as const, text: 'Use either state or states, not both.' }],
        isError: true,
      };
    }

    const query: UpbitQuery = {
      page: args.page,
      order_by: args.order_by,
      limit: args.limit,
    };

    const market = normalizeOptionalString(args.market);
    if (market) {
      query.market = market.toUpperCase();
    }
    if (args.states && args.states.length > 0) {
      query['states[]'] = args.states;
    } else if (args.state) {
      query.state = args.state;
    } else {
      query['states[]'] = ['wait', 'watch'];
    }

    try {
      const payload = await requestUpbitPrivate('/v1/orders/open', {
        method: 'GET',
        query,
        rateLimitGroupHint: 'default',
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected open orders response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitOrderRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No open orders found.' }] };
      }

      const lines = rows
        .slice(0, args.limit)
        .map((row) => formatUpbitOrderLine(row))
        .join('\n');
      const suffix = rows.length > args.limit ? `\n... (${rows.length - args.limit} more)` : '';

      return {
        content: [{ type: 'text' as const, text: `Upbit open orders:\n${lines}${suffix}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit open orders', error);
    }
  },
);

server.tool(
  'upbit_get_order_chance',
  'Fetch authenticated order availability/policy information for a market from Upbit exchange API.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
  },
  async (args) => {
    const market = args.market.trim().toUpperCase();
    if (!market) {
      return {
        content: [{ type: 'text' as const, text: 'market is required.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPrivate('/v1/orders/chance', {
        method: 'GET',
        query: { market },
        rateLimitGroupHint: 'default',
      });
      const row = (Array.isArray(payload) ? payload[0] : payload) as UpbitOrderChancePayload | undefined;
      if (!row || typeof row !== 'object') {
        return {
          content: [{ type: 'text' as const, text: `Unexpected order chance response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const marketId = row.market?.id || market;
      const marketState = row.market?.state || 'unknown';
      const bidTypes = Array.isArray(row.market?.bid_types) ? row.market?.bid_types.join(', ') : '-';
      const askTypes = Array.isArray(row.market?.ask_types) ? row.market?.ask_types.join(', ') : '-';
      const bidMin = row.market?.bid?.min_total || '-';
      const askMin = row.market?.ask?.min_total || '-';
      const maxTotal = row.market?.max_total || '-';
      const lines = [
        `- market=${marketId}, state=${marketState}`,
        `- bid_types=${bidTypes}`,
        `- ask_types=${askTypes}`,
        `- min_total(bid)=${bidMin}, min_total(ask)=${askMin}, max_total=${maxTotal}`,
        `- fee(bid)=${row.bid_fee || '-'}, fee(ask)=${row.ask_fee || '-'}`,
      ].join('\n');

      return {
        content: [{ type: 'text' as const, text: `Upbit order chance:\n${lines}\n\nraw:\n${formatUpbitJson(payload)}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit order chance', error);
    }
  },
);

server.tool(
  'upbit_get_order',
  'Fetch one authenticated order by uuid or identifier from Upbit exchange API.',
  {
    uuid: z.string().optional().describe('Upbit order UUID.'),
    identifier: z.string().optional().describe('Client-defined order identifier.'),
  },
  async (args) => {
    const selector = buildOrderSelectorQuery(args.uuid, args.identifier);
    if (selector.error || !selector.query) {
      return {
        content: [{ type: 'text' as const, text: selector.error || 'Invalid order selector.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPrivate('/v1/order', {
        method: 'GET',
        query: selector.query,
        rateLimitGroupHint: 'default',
      });
      const row = (Array.isArray(payload) ? payload[0] : payload) as UpbitOrderRow | undefined;
      if (!row || typeof row !== 'object') {
        return {
          content: [{ type: 'text' as const, text: `Unexpected order response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      return {
        content: [{ type: 'text' as const, text: `Upbit order:\n${formatUpbitOrderLine(row)}\n\nraw:\n${formatUpbitJson(payload)}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit order', error);
    }
  },
);

server.tool(
  'upbit_get_closed_orders',
  'Fetch authenticated closed orders(done/cancel) from Upbit exchange API.',
  {
    market: z.string().optional().describe('Optional market code filter (e.g. KRW-BTC).'),
    state: z
      .enum(['done', 'cancel'])
      .optional()
      .describe('Optional single state filter. Cannot be used with states.'),
    states: z
      .array(z.enum(['done', 'cancel']))
      .min(1)
      .max(2)
      .optional()
      .describe('Optional multi-state filter. Cannot be used with state.'),
    start_time: z
      .string()
      .optional()
      .describe('Optional start time filter (ISO 8601 with timezone or millisecond timestamp string).'),
    end_time: z
      .string()
      .optional()
      .describe('Optional end time filter (ISO 8601 with timezone or millisecond timestamp string).'),
    limit: z.number().int().min(1).max(1000).default(100).describe('Maximum rows to print.'),
    order_by: z.enum(['asc', 'desc']).default('desc').describe('Sort direction by creation time.'),
  },
  async (args) => {
    if (args.state && args.states && args.states.length > 0) {
      return {
        content: [{ type: 'text' as const, text: 'Use either state or states, not both.' }],
        isError: true,
      };
    }

    const query: UpbitQuery = {
      order_by: args.order_by,
      limit: args.limit,
    };

    const market = normalizeOptionalString(args.market);
    if (market) {
      query.market = market.toUpperCase();
    }
    const startTime = normalizeOptionalString(args.start_time);
    if (startTime) {
      query.start_time = startTime;
    }
    const endTime = normalizeOptionalString(args.end_time);
    if (endTime) {
      query.end_time = endTime;
    }
    if (args.states && args.states.length > 0) {
      query['states[]'] = args.states;
    } else if (args.state) {
      query.state = args.state;
    }

    try {
      const payload = await requestUpbitPrivate('/v1/orders/closed', {
        method: 'GET',
        query,
        rateLimitGroupHint: 'default',
      });
      if (!Array.isArray(payload)) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected closed orders response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }

      const rows = payload as UpbitOrderRow[];
      if (rows.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No closed orders found.' }] };
      }

      const lines = rows
        .slice(0, args.limit)
        .map((row) => formatUpbitOrderLine(row))
        .join('\n');
      const suffix = rows.length > args.limit ? `\n... (${rows.length - args.limit} more)` : '';

      return {
        content: [{ type: 'text' as const, text: `Upbit closed orders:\n${lines}${suffix}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to fetch Upbit closed orders', error);
    }
  },
);

server.tool(
  'upbit_cancel_order',
  'Cancel one authenticated open order by uuid or identifier.',
  {
    uuid: z.string().optional().describe('Upbit order UUID.'),
    identifier: z.string().optional().describe('Client-defined order identifier.'),
    confirm_cancel: z
      .boolean()
      .default(false)
      .describe('Must be true to execute cancel request.'),
  },
  async (args) => {
    if (!args.confirm_cancel) {
      return {
        content: [{ type: 'text' as const, text: 'Order cancel is blocked until confirm_cancel=true.' }],
        isError: true,
      };
    }

    const selector = buildOrderSelectorQuery(args.uuid, args.identifier);
    if (selector.error || !selector.query) {
      return {
        content: [{ type: 'text' as const, text: selector.error || 'Invalid order selector.' }],
        isError: true,
      };
    }

    try {
      const payload = await requestUpbitPrivate('/v1/order', {
        method: 'DELETE',
        query: selector.query,
        rateLimitGroupHint: 'default',
        maxRetries: 0,
      });
      const rows = (Array.isArray(payload) ? payload : [payload]).filter(
        (row): row is UpbitOrderRow => !!row && typeof row === 'object',
      );
      if (rows.length === 0) {
        return {
          content: [{ type: 'text' as const, text: `Unexpected cancel order response: ${formatUpbitJson(payload)}` }],
          isError: true,
        };
      }
      const lines = rows.map((row) => formatUpbitOrderLine(row)).join('\n');

      return {
        content: [{ type: 'text' as const, text: `Upbit cancel order response:\n${lines}\n\nraw:\n${formatUpbitJson(payload)}` }],
      };
    } catch (error) {
      return upbitToolError('Failed to cancel Upbit order', error);
    }
  },
);

const upbitNumericInput = z.union([z.string(), z.number()]).transform((value) => String(value));

server.tool(
  'upbit_create_order',
  'Create or test an Upbit order. Default is dry-run(test) mode. Real order submission requires explicit confirmation.',
  {
    market: z.string().describe('Market code, e.g. KRW-BTC.'),
    side: z.enum(['bid', 'ask']).describe('bid=buy, ask=sell'),
    ord_type: z.enum(['limit', 'price', 'market', 'best']).describe('Order type.'),
    volume: upbitNumericInput
      .optional()
      .describe('Order volume. Required for limit, market ask, best ask.'),
    price: upbitNumericInput
      .optional()
      .describe('Order price/amount. Required for limit, price bid, best bid.'),
    time_in_force: z
      .enum(['ioc', 'fok', 'post_only'])
      .optional()
      .describe('Optional for limit, required(ioc/fok) for best.'),
    smp_type: z
      .enum(['cancel_maker', 'cancel_taker', 'reduce'])
      .optional()
      .describe('Self-match prevention option.'),
    identifier: z.string().optional().describe('Client-defined unique identifier.'),
    dry_run: z
      .boolean()
      .default(true)
      .describe('When true, calls /v1/orders/test without placing real order.'),
    confirm_real_order: z
      .boolean()
      .default(false)
      .describe('Must be true to place a real order when dry_run is false.'),
  },
  async (args) => {
    const payload: UpbitCreateOrderPayload = {
      market: args.market.trim().toUpperCase(),
      side: args.side,
      ord_type: args.ord_type,
      volume: normalizeOptionalString(args.volume),
      price: normalizeOptionalString(args.price),
      time_in_force: args.time_in_force,
      smp_type: args.smp_type,
      identifier: normalizeOptionalString(args.identifier),
    };
    const errors = validateUpbitOrderPayload(payload);
    if (errors.length > 0) {
      return {
        content: [
          {
            type: 'text' as const,
            text: `Order validation failed:\n- ${errors.join('\n- ')}`,
          },
        ],
        isError: true,
      };
    }

    if (!args.dry_run && !args.confirm_real_order) {
      return {
        content: [
          {
            type: 'text' as const,
            text:
              'Real order submission is blocked until explicitly confirmed. Re-run with dry_run=false and confirm_real_order=true if you really want to place it.',
          },
        ],
        isError: true,
      };
    }
    if (!args.dry_run && !payload.identifier) {
      return {
        content: [
          {
            type: 'text' as const,
            text:
              'Real order requires a stable identifier for idempotency. Re-run with identifier=<unique_key>.',
          },
        ],
        isError: true,
      };
    }

    const endpoint = args.dry_run ? '/v1/orders/test' : '/v1/orders';
    const modeLabel = args.dry_run ? 'test-order' : 'real-order';
    const rateGroup = args.dry_run ? 'order-test' : 'order';
    const body = buildUpbitOrderBody(payload);

    try {
      const response = await requestUpbitPrivate(endpoint, {
        method: 'POST',
        body,
        rateLimitGroupHint: rateGroup,
        maxRetries: args.dry_run ? UPBIT_DEFAULT_MAX_RETRIES : 0,
        intent: {
          confirmRealOrder: args.confirm_real_order,
        },
      });

      return {
        content: [
          {
            type: 'text' as const,
            text: `Upbit ${modeLabel} response:\n${formatUpbitJson(response)}`,
          },
        ],
      };
    } catch (error) {
      return upbitToolError(`Failed to submit Upbit ${modeLabel}`, error);
    }
  },
);

server.tool(
  'schedule_task',
  `Schedule a recurring or one-time task. The task will run as a full agent with access to all tools.

CONTEXT MODE - Choose based on task type:
\u2022 "group": Task runs in the group's conversation context, with access to chat history. Use for tasks that need context about ongoing discussions, user preferences, or recent interactions.
\u2022 "isolated": Task runs in a fresh session with no conversation history. Use for independent tasks that don't need prior context. When using isolated mode, include all necessary context in the prompt itself.

If unsure which mode to use, you can ask the user. Examples:
- "Remind me about our discussion" \u2192 group (needs conversation context)
- "Check the weather every morning" \u2192 isolated (self-contained task)
- "Follow up on my request" \u2192 group (needs to know what was requested)
- "Generate a daily report" \u2192 isolated (just needs instructions in prompt)

MESSAGING BEHAVIOR - The task agent's output is sent to the user or group. It can also use send_message for immediate delivery, or wrap output in <internal> tags to suppress it. Include guidance in the prompt about whether the agent should:
\u2022 Always send a message (e.g., reminders, daily briefings)
\u2022 Only send a message when there's something to report (e.g., "notify me if...")
\u2022 Never send a message (background maintenance tasks)

SCHEDULE VALUE FORMAT:
\u2022 cron: Standard cron expression (e.g., "*/5 * * * *" for every 5 minutes, "0 9 * * *" for daily at 9am LOCAL time)
\u2022 interval: Milliseconds between runs (e.g., "300000" for 5 minutes, "3600000" for 1 hour)
\u2022 once: Use either local time WITHOUT "Z" suffix (e.g., "2026-02-01T15:30:00"), or include an explicit offset (e.g., "2026-02-01T15:30:00+09:00"). If the user specifies a timezone, prefer explicit offset and do NOT convert then drop timezone info.`,
  {
    prompt: z.string().describe('What the agent should do when the task runs. For isolated mode, include all necessary context here.'),
    schedule_type: z.enum(['cron', 'interval', 'once']).describe('cron=recurring at specific times, interval=recurring every N ms, once=run once at specific time'),
    schedule_value: z.string().describe('cron: "*/5 * * * *" | interval: milliseconds like "300000" | once: "2026-02-01T15:30:00" (local) or "2026-02-01T15:30:00+09:00" (explicit offset); avoid UTC/Z unless user wants UTC'),
    context_mode: z.enum(['group', 'isolated']).default('group').describe('group=runs with chat history and memory, isolated=fresh session (include context in prompt)'),
    target_group_jid: z.string().optional().describe('(Main group only) JID of the group to schedule the task for. Defaults to the current group.'),
  },
  async (args) => {
    // Validate schedule_value before writing IPC
    if (args.schedule_type === 'cron') {
      try {
        CronExpressionParser.parse(args.schedule_value);
      } catch {
        return {
          content: [{ type: 'text' as const, text: `Invalid cron: "${args.schedule_value}". Use format like "0 9 * * *" (daily 9am) or "*/5 * * * *" (every 5 min).` }],
          isError: true,
        };
      }
    } else if (args.schedule_type === 'interval') {
      const ms = parseInt(args.schedule_value, 10);
      if (isNaN(ms) || ms <= 0) {
        return {
          content: [{ type: 'text' as const, text: `Invalid interval: "${args.schedule_value}". Must be positive milliseconds (e.g., "300000" for 5 min).` }],
          isError: true,
        };
      }
    } else if (args.schedule_type === 'once') {
      const date = new Date(args.schedule_value);
      if (isNaN(date.getTime())) {
        return {
          content: [{ type: 'text' as const, text: `Invalid timestamp: "${args.schedule_value}". Use local ISO 8601 like "2026-02-01T15:30:00" (no Z suffix).` }],
          isError: true,
        };
      }
    }

    // Non-main groups can only schedule for themselves
    const targetJid = isMain && args.target_group_jid ? args.target_group_jid : chatJid;

    const data = {
      type: 'schedule_task',
      prompt: args.prompt,
      schedule_type: args.schedule_type,
      schedule_value: args.schedule_value,
      context_mode: args.context_mode || 'group',
      targetJid,
      createdBy: groupFolder,
      timestamp: new Date().toISOString(),
    };

    const filename = writeIpcFile(TASKS_DIR, data);

    return {
      content: [{ type: 'text' as const, text: `Task scheduled (${filename}): ${args.schedule_type} - ${args.schedule_value}` }],
    };
  },
);

server.tool(
  'list_tasks',
  "List all scheduled tasks. From main: shows all tasks. From other groups: shows only that group's tasks.",
  {},
  async () => {
    const tasksFile = path.join(IPC_DIR, 'current_tasks.json');

    try {
      if (!fs.existsSync(tasksFile)) {
        return { content: [{ type: 'text' as const, text: 'No scheduled tasks found.' }] };
      }

      const allTasks = JSON.parse(fs.readFileSync(tasksFile, 'utf-8'));

      const tasks = isMain
        ? allTasks
        : allTasks.filter((t: { groupFolder: string }) => t.groupFolder === groupFolder);

      if (tasks.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No scheduled tasks found.' }] };
      }

      const formatted = tasks
        .map(
          (t: { id: string; prompt: string; schedule_type: string; schedule_value: string; status: string; next_run: string }) =>
            `- [${t.id}] ${t.prompt.slice(0, 50)}... (${t.schedule_type}: ${t.schedule_value}) - ${t.status}, next: ${t.next_run || 'N/A'}`,
        )
        .join('\n');

      return { content: [{ type: 'text' as const, text: `Scheduled tasks:\n${formatted}` }] };
    } catch (err) {
      return {
        content: [{ type: 'text' as const, text: `Error reading tasks: ${err instanceof Error ? err.message : String(err)}` }],
      };
    }
  },
);

server.tool(
  'pause_task',
  'Pause a scheduled task. It will not run until resumed.',
  { task_id: z.string().describe('The task ID to pause') },
  async (args) => {
    const data = {
      type: 'pause_task',
      taskId: args.task_id,
      groupFolder,
      isMain,
      timestamp: new Date().toISOString(),
    };

    writeIpcFile(TASKS_DIR, data);

    return { content: [{ type: 'text' as const, text: `Task ${args.task_id} pause requested.` }] };
  },
);

server.tool(
  'resume_task',
  'Resume a paused task.',
  { task_id: z.string().describe('The task ID to resume') },
  async (args) => {
    const data = {
      type: 'resume_task',
      taskId: args.task_id,
      groupFolder,
      isMain,
      timestamp: new Date().toISOString(),
    };

    writeIpcFile(TASKS_DIR, data);

    return { content: [{ type: 'text' as const, text: `Task ${args.task_id} resume requested.` }] };
  },
);

server.tool(
  'cancel_task',
  'Cancel and delete a scheduled task.',
  { task_id: z.string().describe('The task ID to cancel') },
  async (args) => {
    const data = {
      type: 'cancel_task',
      taskId: args.task_id,
      groupFolder,
      isMain,
      timestamp: new Date().toISOString(),
    };

    writeIpcFile(TASKS_DIR, data);

    return { content: [{ type: 'text' as const, text: `Task ${args.task_id} cancellation requested.` }] };
  },
);

server.tool(
  'register_group',
  `Register a new WhatsApp group so the agent can respond to messages there. Main group only.

Use available_groups.json to find the JID for a group. The folder name should be lowercase with hyphens (e.g., "family-chat").`,
  {
    jid: z.string().describe('The WhatsApp JID (e.g., "120363336345536173@g.us")'),
    name: z.string().describe('Display name for the group'),
    folder: z.string().describe('Folder name for group files (lowercase, hyphens, e.g., "family-chat")'),
    trigger: z.string().describe('Trigger word (e.g., "@Andy")'),
  },
  async (args) => {
    if (!isMain) {
      return {
        content: [{ type: 'text' as const, text: 'Only the main group can register new groups.' }],
        isError: true,
      };
    }

    const data = {
      type: 'register_group',
      jid: args.jid,
      name: args.name,
      folder: args.folder,
      trigger: args.trigger,
      timestamp: new Date().toISOString(),
    };

    writeIpcFile(TASKS_DIR, data);

    return {
      content: [{ type: 'text' as const, text: `Group "${args.name}" registered. It will start receiving messages immediately.` }],
    };
  },
);

server.tool(
  'refresh_groups',
  'Refresh and return the current available group list. Main group only.',
  {},
  async () => {
    if (!isMain) {
      return {
        content: [{ type: 'text' as const, text: 'Only the main group can refresh available groups.' }],
        isError: true,
      };
    }

    const data = {
      type: 'refresh_groups',
      timestamp: new Date().toISOString(),
    };
    writeIpcFile(TASKS_DIR, data);

    const groupsFile = path.join(IPC_DIR, 'available_groups.json');
    try {
      if (!fs.existsSync(groupsFile)) {
        return { content: [{ type: 'text' as const, text: 'Group refresh requested.' }] };
      }
      const payload = JSON.parse(fs.readFileSync(groupsFile, 'utf-8')) as {
        groups?: Array<{ jid: string; name?: string }>;
      };
      const groups = Array.isArray(payload.groups) ? payload.groups : [];
      if (groups.length === 0) {
        return { content: [{ type: 'text' as const, text: 'Group refresh requested. No groups found.' }] };
      }
      const lines = groups.map((group) => `- ${group.name || 'Unknown'} (${group.jid})`).join('\n');
      return { content: [{ type: 'text' as const, text: `Group refresh requested.\n${lines}` }] };
    } catch (err) {
      return { content: [{ type: 'text' as const, text: `Group refresh requested. (${err instanceof Error ? err.message : String(err)})` }] };
    }
  },
);

server.tool(
  'list_skills',
  `List SKILL.md-based capabilities currently visible to this group.

- Non-main groups: shows this group's skills only.
- Main group: can show all groups, or filter with target_group_folder.`,
  {
    target_group_folder: z.string().optional().describe('(Main group only) Filter skills by group folder.'),
  },
  async (args) => {
    const target = args.target_group_folder?.trim();
    if (!isMain && target && target !== groupFolder) {
      return {
        content: [{ type: 'text' as const, text: 'Only the main group can inspect another group\'s skills.' }],
        isError: true,
      };
    }

    await requestSkillsRefresh(target);
    const snapshot = await waitForSkillSnapshot();
    if (!snapshot) {
      return { content: [{ type: 'text' as const, text: 'No skill snapshot available yet.' }] };
    }

    const skills = listableSkills(snapshot, target);
    if (skills.length === 0) {
      return { content: [{ type: 'text' as const, text: 'No skills found.' }] };
    }

    const formatted = skills
      .map((skill) => {
        const group = skill.groupFolder || groupFolder;
        const name = (skill.name || '').trim() || '(unnamed)';
        const source = skill.source || 'unknown';
        const state = skill.enabled === false ? 'disabled' : 'enabled';
        const location = skill.path ? ` @ ${skill.path}` : '';
        const groupLabel = isMain ? ` [group: ${group}]` : '';
        return `- ${name}${groupLabel} (${source}, ${state})${location}`;
      })
      .join('\n');

    return {
      content: [{ type: 'text' as const, text: `Skills${snapshot.lastSync ? ` (snapshot: ${snapshot.lastSync})` : ''}:\n${formatted}` }],
    };
  },
);

server.tool(
  'enable_skill',
  'Enable a skill for a group. Default target is the current group.',
  {
    skill_name: z.string().describe('Skill folder/name to enable (case-insensitive).'),
    target_group_folder: z.string().optional().describe('(Main group only) Group folder to apply this setting to.'),
  },
  async (args) => {
    const target = args.target_group_folder?.trim();
    if (!isMain && target && target !== groupFolder) {
      return {
        content: [{ type: 'text' as const, text: 'Only the main group can change another group\'s skills.' }],
        isError: true,
      };
    }
    const resolvedTarget = isMain && target ? target : groupFolder;

    writeIpcFile(TASKS_DIR, {
      type: 'enable_skill',
      skillName: args.skill_name,
      targetGroupFolder: resolvedTarget,
      timestamp: new Date().toISOString(),
    });
    await requestSkillsRefresh(resolvedTarget);

    return {
      content: [{ type: 'text' as const, text: `Skill "${args.skill_name}" enabled for group "${resolvedTarget}".` }],
    };
  },
);

server.tool(
  'disable_skill',
  'Disable a skill for a group. Default target is the current group.',
  {
    skill_name: z.string().describe('Skill folder/name to disable (case-insensitive).'),
    target_group_folder: z.string().optional().describe('(Main group only) Group folder to apply this setting to.'),
  },
  async (args) => {
    const target = args.target_group_folder?.trim();
    if (!isMain && target && target !== groupFolder) {
      return {
        content: [{ type: 'text' as const, text: 'Only the main group can change another group\'s skills.' }],
        isError: true,
      };
    }
    const resolvedTarget = isMain && target ? target : groupFolder;

    writeIpcFile(TASKS_DIR, {
      type: 'disable_skill',
      skillName: args.skill_name,
      targetGroupFolder: resolvedTarget,
      timestamp: new Date().toISOString(),
    });
    await requestSkillsRefresh(resolvedTarget);

    return {
      content: [{ type: 'text' as const, text: `Skill "${args.skill_name}" disabled for group "${resolvedTarget}".` }],
    };
  },
);

// Start the stdio transport
const transport = new StdioServerTransport();
await server.connect(transport);
