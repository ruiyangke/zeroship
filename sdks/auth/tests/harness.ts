/**
 * Hand-rolled DOM/Web-API fakes for the @zeroship/auth client tests.
 *
 * These drive the REAL cache / CacheManager / navigator.locks serialization /
 * popup postMessage handshake / transport / breadcrumb logic — nothing under
 * test is stubbed. Only the browser BOUNDARY (window, fetch, storage, locks,
 * crypto, BroadcastChannel, document.cookie) is faked, exactly the surface the
 * client reads through its injectable {@link ClientEnv}.
 */

import type {
  BroadcastChannelLike,
  ClientEnv,
  CookieJar,
  CryptoLike,
  LockManagerLike,
  MessageEventLike,
  StorageEventLike,
  StorageLike,
  WindowLike,
  WindowProxyLike,
} from "../src/internal/env";

export const APP_ORIGIN = "https://myapp.zeroship.ai";

/** In-memory Storage matching the Storage interface. */
export class FakeStorage implements StorageLike {
  readonly map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.has(key) ? this.map.get(key)! : null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, String(value));
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
  key(index: number): string | null {
    return Array.from(this.map.keys())[index] ?? null;
  }
  get length(): number {
    return this.map.size;
  }
}

/** Deterministic crypto: getRandomValues fills a counter; subtle.digest hashes via a stable fold. */
export class FakeCrypto implements CryptoLike {
  private counter = 1;
  getRandomValues<T extends ArrayBufferView>(array: T): T {
    const view = new Uint8Array(array.buffer, array.byteOffset, array.byteLength);
    for (let i = 0; i < view.length; i++) view[i] = (this.counter++ * 31 + i) & 0xff;
    return array;
  }
  subtle = {
    // A stable, non-cryptographic 32-byte digest is sufficient for the SDK
    // unit tests (the gateway is the only party that verifies the challenge;
    // here we assert the SDK derives *a* deterministic S256 from the verifier).
    async digest(_alg: "SHA-256", data: ArrayBuffer | ArrayBufferView): Promise<ArrayBuffer> {
      const bytes =
        data instanceof ArrayBuffer
          ? new Uint8Array(data)
          : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      const out = new Uint8Array(32);
      for (let i = 0; i < bytes.length; i++) out[i % 32] = (out[i % 32] + bytes[i] * 17 + i) & 0xff;
      return out.buffer;
    },
  };
}

/** A cookie jar honoring Max-Age=0 deletion and name=value upserts. */
export class FakeCookies implements CookieJar {
  private jar = new Map<string, string>();
  get(): string {
    return Array.from(this.jar.entries())
      .map(([k, v]) => `${k}=${v}`)
      .join("; ");
  }
  set(value: string): void {
    const [pair, ...attrs] = value.split(";").map((s) => s.trim());
    const eq = pair.indexOf("=");
    const name = pair.slice(0, eq);
    const val = pair.slice(eq + 1);
    const maxAge = attrs.find((a) => a.toLowerCase().startsWith("max-age="));
    if (maxAge && maxAge.split("=")[1] === "0") {
      this.jar.delete(name);
    } else {
      this.jar.set(name, val);
    }
  }
}

/** A fake popup window whose `closed` flips on demand. */
export class FakePopup implements WindowProxyLike {
  closed = false;
  location = { href: "" };
  focus(): void {}
  close(): void {
    this.closed = true;
  }
}

/** A real navigator.locks-style manager: serializes by name. */
export class FakeLocks implements LockManagerLike {
  private chains = new Map<string, Promise<unknown>>();
  /** Observable acquisition order, to assert serialization. */
  readonly order: string[] = [];
  async request<T>(
    name: string,
    _options: { signal?: AbortSignal },
    callback: () => Promise<T>,
  ): Promise<T> {
    const prior = this.chains.get(name) ?? Promise.resolve();
    let release!: () => void;
    const next = new Promise<void>((r) => (release = r));
    this.chains.set(name, next);
    await prior;
    this.order.push(`acquire:${name}`);
    try {
      return await callback();
    } finally {
      this.order.push(`release:${name}`);
      release();
    }
  }
}

/** A same-isolate BroadcastChannel bus keyed by channel name. */
class BroadcastBus {
  private channels = new Map<string, Set<FakeBroadcastChannel>>();
  register(ch: FakeBroadcastChannel): void {
    let set = this.channels.get(ch.name);
    if (!set) this.channels.set(ch.name, (set = new Set()));
    set.add(ch);
  }
  unregister(ch: FakeBroadcastChannel): void {
    this.channels.get(ch.name)?.delete(ch);
  }
  publish(from: FakeBroadcastChannel, message: unknown): void {
    for (const ch of this.channels.get(from.name) ?? []) {
      // Standard BroadcastChannel does not echo to the sender.
      if (ch !== from && ch.onmessage) ch.onmessage({ data: message });
    }
  }
}
class FakeBroadcastChannel implements BroadcastChannelLike {
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  constructor(
    readonly name: string,
    private readonly bus: BroadcastBus,
  ) {
    bus.register(this);
  }
  postMessage(message: unknown): void {
    this.bus.publish(this, message);
  }
  close(): void {
    this.bus.unregister(this);
  }
}

/**
 * The fake window. Captures `open()` returns, dispatches `message` events to
 * registered listeners, and exposes real timers (node globals) so the popup
 * poll/timeout and backoff delays run.
 */
export class FakeWindow implements WindowLike {
  lastOpened?: FakePopup;
  openReturnsNull = false;
  location = { href: "" };
  readonly Worker?: unknown;
  private messageListeners = new Set<(ev: MessageEventLike) => void>();

  constructor(opts?: { hasWorker?: boolean }) {
    if (opts?.hasWorker) this.Worker = class {};
  }

  open(_url: string | URL, _target?: string): WindowProxyLike | null {
    if (this.openReturnsNull) return null;
    this.lastOpened = new FakePopup();
    return this.lastOpened;
  }
  addEventListener(_type: "message", listener: (ev: MessageEventLike) => void): void {
    this.messageListeners.add(listener);
  }
  /** Number of installed `message` listeners (relay readiness probe). */
  get messageListenerCount(): number {
    return this.messageListeners.size;
  }
  removeEventListener(_type: "message", listener: (ev: MessageEventLike) => void): void {
    this.messageListeners.delete(listener);
  }
  /** Drive a postMessage into the SDK's relay listener. */
  dispatchMessage(ev: MessageEventLike): void {
    for (const l of [...this.messageListeners]) l(ev);
  }
  setTimeout(handler: () => void, timeout?: number): number {
    return setTimeout(handler, timeout) as unknown as number;
  }
  clearTimeout(id: number): void {
    clearTimeout(id as unknown as NodeJS.Timeout);
  }
  setInterval(handler: () => void, timeout?: number): number {
    return setInterval(handler, timeout) as unknown as number;
  }
  clearInterval(id: number): void {
    clearInterval(id as unknown as NodeJS.Timeout);
  }
}

/** A recorded fetch call (for assertions). */
export interface RecordedRequest {
  url: string;
  method: string;
  headers: Record<string, string>;
  body: unknown;
}

/** Programmable fetch: a queue of responders matched by `url` substring. */
export class FakeFetch {
  readonly requests: RecordedRequest[] = [];
  private routes: Array<{
    match: (url: string, method: string) => boolean;
    handler: (req: RecordedRequest) => Response | Promise<Response>;
  }> = [];

  /**
   * Register a responder. `match` is either a URL substring or a predicate
   * `(url, method) => boolean`. The `method` arg lets a test disambiguate the
   * merged `/__zs/auth/session` resource (POST exchange vs GET probe) on the
   * SAME path — first-match-wins by registration order.
   */
  on(
    match: string | ((url: string, method: string) => boolean),
    handler: (req: RecordedRequest) => Response | Promise<Response>,
  ): this {
    const m = typeof match === "string" ? (u: string) => u.includes(match) : match;
    this.routes.push({ match: m, handler });
    return this;
  }

  readonly fetch: typeof fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = (init?.method ?? "GET").toUpperCase();
    const headers: Record<string, string> = {};
    const h = init?.headers as Record<string, string> | undefined;
    if (h) for (const [k, v] of Object.entries(h)) headers[k.toLowerCase()] = v as string;
    let body: unknown = init?.body;
    if (typeof body === "string") {
      try {
        body = JSON.parse(body);
      } catch {
        /* keep raw */
      }
    }
    const rec: RecordedRequest = { url, method, headers, body };
    this.requests.push(rec);
    const route = this.routes.find((r) => r.match(url, method));
    if (!route) throw new Error(`FakeFetch: no route for ${rec.method} ${url}`);
    return route.handler(rec);
  };
}

/**
 * Matcher for the BFF code→session exchange: `POST /__zs/auth/session` (NOT the
 * GET probe / `?mint=1`). Use this where a test registers both the exchange and
 * a GET `/session` responder on the merged resource.
 */
export const SESSION_EXCHANGE = (u: string, method: string): boolean =>
  method === "POST" && u.includes("/__zs/auth/session");

/** Build a JSON `Response`. */
export function jsonResponse(status: number, body: unknown, headers?: Record<string, string>): Response {
  return new Response(status === 204 ? null : JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

/** A complete fake env for the client, with handles exposed for assertions. */
export interface Harness {
  env: ClientEnv;
  window: FakeWindow;
  fetch: FakeFetch;
  session: FakeStorage;
  local: FakeStorage;
  cookies: FakeCookies;
  locks: FakeLocks;
  crypto: FakeCrypto;
  broadcast: BroadcastBus;
  /** Trigger a one-shot localStorage relay storage-event. */
  fireStorage(key: string, newValue: string): void;
}

export function makeHarness(opts?: { hasWorker?: boolean; withLocks?: boolean }): Harness {
  const window = new FakeWindow({ hasWorker: opts?.hasWorker });
  const fetch = new FakeFetch();
  const session = new FakeStorage();
  const local = new FakeStorage();
  const cookies = new FakeCookies();
  const locks = new FakeLocks();
  const crypto = new FakeCrypto();
  const bus = new BroadcastBus();
  const storageListeners = new Set<(ev: StorageEventLike) => void>();

  const env: ClientEnv = {
    window,
    fetch: fetch.fetch,
    session,
    local,
    cookies,
    crypto,
    location: { origin: APP_ORIGIN },
    locks: opts?.withLocks === false ? undefined : locks,
    broadcastChannel: (name: string) => new FakeBroadcastChannel(name, bus),
    onStorage: (listener) => {
      storageListeners.add(listener);
      return () => storageListeners.delete(listener);
    },
    // Short timings so popup poll/timeout-driven tests resolve fast and never
    // leak a 60 s timer into the node:test runner.
    popupTiming: { pollMs: 5, timeoutMs: 300 },
  };

  return {
    env,
    window,
    fetch,
    session,
    local,
    cookies,
    locks,
    crypto,
    broadcast: bus,
    fireStorage(key: string, newValue: string) {
      for (const l of [...storageListeners]) l({ key, newValue });
    },
  };
}

/**
 * Standard `POST /__zs/auth/session` success body the gateway returns (BFF slice
 * R1b — identity projection ONLY; NO `access_token`/`token_type`/`scope` in the
 * body, the credential is the HttpOnly signed cookie). The `scopes` ride inside
 * the `user` projection.
 */
export function tokenSuccessBody(over?: Partial<Record<string, unknown>>) {
  return {
    user: {
      id: "pws_alice",
      email: "alice@relay.zeroship.ai",
      email_verified: true,
      name: "Alice",
      avatar: null,
      scopes: ["openid", "profile", "email"],
    },
    expires_at: Math.floor(Date.now() / 1000) + 600,
    ...over,
  };
}
