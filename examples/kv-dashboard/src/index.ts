"use server";

import { kv, type ListResult, type Result } from "@zeroship/kv";
import { mutation, query } from "@zeroship/rpc/server";

const PREFIX = "kv-demo:";
const VISITS_KEY = "counter:visits";
const FLAG_KEY = "flags:checkout";
const LEASE_KEY = "leases:deploy";
const TEXT_KEY = "strings:greeting";
const RATE_LIMIT = 5;
const RATE_WINDOW_MS = 60_000;
const SESSION_TTL_MS = 120_000;
const LEASE_TTL_MS = 30_000;
const CACHE_TTL_MS = 30_000;
const TEXT_TTL_MS = 120_000;
const SESSION_LIST_PAGE_SIZE = 50;
const SESSION_DISPLAY_LIMIT = 100;

type Flag = {
  enabled: boolean;
  updatedAt: number;
};

const DEFAULT_FLAG: Flag = {
  enabled: false,
  updatedAt: 0,
};

export type Quote = {
  sku: string;
  price: number;
  currency: "USD";
  generatedAt: number;
};

export type LeaseInfo = {
  owner: string;
  acquiredAt: number;
  leaseId: string;
};

export type SessionInfo = {
  token: string;
  name: string;
  createdAt: number;
  ttlMs: number | null;
};

export type RateLimitResult = {
  actor: string;
  allowed: boolean;
  count: number;
  remaining: number;
  resetMs: number | null;
};

export type QuoteResult = {
  quote: Quote;
  source: "hit" | "miss";
  ttlMs: number | null;
};

export type LeaseResult = {
  acquired: boolean;
  lease: (LeaseInfo & { ttlMs: number | null }) | null;
};

export type KeysPage = {
  keys: string[];
  cursor: string | null;
};

export type StringValueResult = {
  value: string | null;
  has: boolean;
  ttlMs: number | null;
};

export type StringMutationResult = StringValueResult & {
  updated?: boolean;
  deleted?: boolean;
};

export type MemoValue = {
  label: string;
  builtAt: number;
  nonce: string;
};

export type MemoResult = {
  value: MemoValue;
  source: "hit" | "miss";
  ttlMs: number | null;
};

export type ListKeysInput = {
  prefix?: string;
  cursor?: string | null;
  limit?: number;
};

export type DashboardSnapshot = {
  visits: number;
  checkoutEnabled: boolean;
  lease: (LeaseInfo & { ttlMs: number | null }) | null;
  sessions: SessionInfo[];
  sessionsTruncated: boolean;
  text: StringValueResult;
  keys: string[];
  generatedAt: number;
};

const store = () => kv.namespace(PREFIX);
const sessions = () => store().namespace("session:");

function must<T>(r: Result<T>): T {
  if (r.error) throw r.error;
  return r.data;
}

function localKey(key: string): string {
  return key.startsWith(PREFIX) ? key.slice(PREFIX.length) : key;
}

function cleanPart(value: string, fallback: string): string {
  const cleaned = value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return (cleaned || fallback).slice(0, 48);
}

function requireText(value: string, field: string): string {
  const text = value.trim();
  if (!text) {
    throw Object.assign(new Error(`${field} is required`), { code: "INVALID_ARGUMENT" });
  }
  if (text.length > 80) {
    throw Object.assign(new Error(`${field} is too long`), { code: "INVALID_ARGUMENT" });
  }
  return text;
}

function keyToken(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function rateWindowKey(actor: string, now = Date.now()): string {
  return `rate:${cleanPart(actor, "guest")}:${Math.floor(now / RATE_WINDOW_MS)}`;
}

function ttlOption(ttlMs?: number | null): { ttlMs?: number } {
  if (ttlMs == null) return {};
  const safe = Math.min(Math.max(1_000, Math.trunc(ttlMs)), 86_400_000);
  return { ttlMs: safe };
}

async function ttlMs(key: string): Promise<number | null> {
  const ttl = must(await store().ttl(key));
  return ttl?.ttlMs ?? null;
}

async function readStringValue(): Promise<StringValueResult> {
  const value = must(await store().getString(TEXT_KEY));
  const has = must(await store().has(TEXT_KEY));
  const ttl = must(await store().ttl(TEXT_KEY));
  return { value, has, ttlMs: ttl?.ttlMs ?? null };
}

async function readFlag(): Promise<Flag> {
  return must(await store().get<Flag>(FLAG_KEY)) ?? DEFAULT_FLAG;
}

async function currentLease(): Promise<(LeaseInfo & { ttlMs: number | null }) | null> {
  const lease = must(await store().get<LeaseInfo>(LEASE_KEY));
  if (!lease) return null;
  return { ...lease, ttlMs: await ttlMs(LEASE_KEY) };
}

async function listSessionInfos(): Promise<{ sessions: SessionInfo[]; truncated: boolean }> {
  const rows: SessionInfo[] = [];
  let cursor: string | null = null;

  do {
    const page: ListResult = must(await sessions().list("", {
      cursor: cursor ?? undefined,
      limit: SESSION_LIST_PAGE_SIZE,
    }));
    for (const key of page.keys) {
      const token = localKey(key).replace(/^session:/, "");
      const session = must(await sessions().get<Omit<SessionInfo, "token" | "ttlMs">>(token));
      if (!session) continue;
      const ttl = must(await sessions().ttl(token));
      rows.push({ token, name: session.name, createdAt: session.createdAt, ttlMs: ttl?.ttlMs ?? null });
      if (rows.length >= SESSION_DISPLAY_LIMIT) {
        return { sessions: rows.sort((a, b) => b.createdAt - a.createdAt), truncated: true };
      }
    }
    cursor = page.cursor;
  } while (cursor);

  return { sessions: rows.sort((a, b) => b.createdAt - a.createdAt), truncated: false };
}

async function collectKeys(prefix = ""): Promise<string[]> {
  const keys = new Set<string>();
  let cursor: string | null = null;

  do {
    const page: ListResult = must(await store().list(prefix, {
      cursor: cursor ?? undefined,
      limit: 100,
    }));
    for (const key of page.keys) keys.add(key);
    cursor = page.cursor;
  } while (cursor);

  return [...keys];
}

async function snapshot(): Promise<DashboardSnapshot> {
  const visits = must(await store().get<number>(VISITS_KEY)) ?? 0;
  const flag = await readFlag();
  const keyPage = must(await store().list("", { limit: 40 }));
  const sessionList = await listSessionInfos();
  return {
    visits,
    checkoutEnabled: flag.enabled,
    lease: await currentLease(),
    sessions: sessionList.sessions,
    sessionsTruncated: sessionList.truncated,
    text: await readStringValue(),
    keys: keyPage.keys,
    generatedAt: Date.now(),
  };
}

export const getSnapshot = query(
  async () => snapshot(),
  { id: "kv.snapshot" },
);

export const recordVisit = mutation(
  async () => {
    must(await store().incr(VISITS_KEY));
    return snapshot();
  },
  { id: "kv.visit" },
);

export const setCheckoutFlag = mutation(
  async ({ enabled }: { enabled: boolean }) => {
    must(await store().set<Flag>(FLAG_KEY, { enabled, updatedAt: Date.now() }));
    return snapshot();
  },
  { id: "kv.flag.set" },
);

export const hitRateLimit = mutation(
  async ({ actor }: { actor: string }): Promise<RateLimitResult> => {
    const safeActor = cleanPart(actor, "guest");
    const key = rateWindowKey(safeActor);
    const count = must(await store().incr(key, { ttlMs: RATE_WINDOW_MS }));
    const resetMs = await ttlMs(key);
    return {
      actor: safeActor,
      allowed: count <= RATE_LIMIT,
      count,
      remaining: Math.max(0, RATE_LIMIT - count),
      resetMs,
    };
  },
  { id: "kv.rate.hit" },
);

export const getQuote = mutation(
  async ({ sku }: { sku: string }): Promise<QuoteResult> => {
    const safeSku = cleanPart(sku, "starter");
    const key = `cache:quote:${safeSku}`;
    const existing = must(await store().get<Quote>(key));
    if (existing) {
      return { quote: existing, source: "hit", ttlMs: await ttlMs(key) };
    }

    const quote: Quote = {
      sku: safeSku,
      price: Number((49 + Math.random() * 450).toFixed(2)),
      currency: "USD",
      generatedAt: Date.now(),
    };
    must(await store().set(key, quote, { ttlMs: CACHE_TTL_MS }));
    return { quote, source: "miss", ttlMs: await ttlMs(key) };
  },
  { id: "kv.cache.quote" },
);

export const getMemo = mutation(
  async ({ label }: { label: string }): Promise<MemoResult> => {
    const safeLabel = cleanPart(label, "welcome");
    const key = `memo:${safeLabel}`;
    const existed = must(await store().has(key));
    const value = must(await store().getOrSet<MemoValue>(
      key,
      { ttlMs: CACHE_TTL_MS },
      () => ({ label: safeLabel, builtAt: Date.now(), nonce: keyToken() }),
    ));
    return { value, source: existed ? "hit" : "miss", ttlMs: await ttlMs(key) };
  },
  { id: "kv.memo.get" },
);

export const acquireLease = mutation(
  async ({ owner }: { owner: string }): Promise<LeaseResult> => {
    const cleanOwner = requireText(owner, "owner");
    const acquiredAt = Date.now();
    const acquired = must(await store().setIfAbsent<LeaseInfo>(
      LEASE_KEY,
      { owner: cleanOwner, acquiredAt, leaseId: keyToken() },
      { ttlMs: LEASE_TTL_MS },
    ));
    return { acquired: acquired.stored, lease: await currentLease() };
  },
  { id: "kv.lease.acquire" },
);

export const clearLease = mutation(
  async (): Promise<LeaseResult> => {
    must(await store().delete(LEASE_KEY));
    return { acquired: false, lease: null };
  },
  { id: "kv.lease.clear" },
);

export const createSession = mutation(
  async ({ name }: { name: string }): Promise<SessionInfo> => {
    const sessionName = requireText(name, "name");
    const token = keyToken();
    const value = { name: sessionName, createdAt: Date.now() };
    must(await sessions().set(token, value, { ttlMs: SESSION_TTL_MS }));
    const ttl = must(await sessions().ttl(token));
    return { token, name: value.name, createdAt: value.createdAt, ttlMs: ttl?.ttlMs ?? null };
  },
  { id: "kv.session.create" },
);

export const deleteSession = mutation(
  async ({ token }: { token: string }) => {
    const safeToken = requireText(token, "token");
    return must(await sessions().delete(safeToken));
  },
  { id: "kv.session.delete" },
);

export const setStringValue = mutation(
  async ({ value, ttlMs = TEXT_TTL_MS }: { value: string; ttlMs?: number | null }): Promise<StringMutationResult> => {
    const text = requireText(value, "value");
    must(await store().set(TEXT_KEY, text, ttlOption(ttlMs)));
    return readStringValue();
  },
  { id: "kv.string.set" },
);

export const expireStringValue = mutation(
  async ({ ttlMs = TEXT_TTL_MS }: { ttlMs?: number }): Promise<StringMutationResult> => {
    const updated = must(await store().expire(TEXT_KEY, ttlOption(ttlMs).ttlMs ?? TEXT_TTL_MS));
    return { ...(await readStringValue()), updated: updated.updated };
  },
  { id: "kv.string.expire" },
);

export const persistStringValue = mutation(
  async (): Promise<StringMutationResult> => {
    const updated = must(await store().persist(TEXT_KEY));
    return { ...(await readStringValue()), updated: updated.updated };
  },
  { id: "kv.string.persist" },
);

export const deleteStringValue = mutation(
  async (): Promise<StringMutationResult> => {
    const deleted = must(await store().delete(TEXT_KEY));
    return { ...(await readStringValue()), deleted: deleted.deleted };
  },
  { id: "kv.string.delete" },
);

export const listKeys = query(
  async ({ prefix = "", cursor = null, limit = 12 }: ListKeysInput): Promise<KeysPage> => {
    const page = must(await store().list(prefix, {
      cursor: cursor ?? undefined,
      limit: Math.min(Math.max(1, limit), 50),
    }));
    return page;
  },
  { id: "kv.keys.list" },
);

export const clearDemo = mutation(
  async () => {
    const keys = await collectKeys();
    let deleted = 0;
    for (const key of keys) {
      const result = must(await store().delete(localKey(key)));
      if (result.deleted) deleted++;
    }
    return { deleted };
  },
  { id: "kv.clear" },
);
