/**
 * Injectable browser environment. Every DOM/Web-API touchpoint the client
 * uses goes through one of these handles so unit tests can drive the REAL
 * popup / relay / transport logic against hand-rolled fakes
 * (no stubbing of the logic under test).
 *
 * In production each field defaults to the corresponding global. In a test
 * the harness passes a `ClientEnv` whose `window`, `fetch`, `storage`,
 * `location`, `crypto`, and `broadcastChannel` are fakes.
 */

/** A minimal `Window`-like handle (only the bits the client reads). */
export interface WindowLike {
  open(url: string | URL, target?: string, features?: string): WindowProxyLike | null;
  addEventListener(
    type: "message",
    listener: (ev: MessageEventLike) => void,
    options?: { signal?: AbortSignal },
  ): void;
  removeEventListener(type: "message", listener: (ev: MessageEventLike) => void): void;
  setTimeout(handler: () => void, timeout?: number): number;
  clearTimeout(id: number): void;
  setInterval(handler: () => void, timeout?: number): number;
  clearInterval(id: number): void;
}

/** The window the popup opens into (only the bits the client reads/sets). */
export interface WindowProxyLike {
  closed: boolean;
  location: { href: string };
  close(): void;
  focus?(): void;
}

/** A `MessageEvent`-like object delivered to a `message` listener. */
export interface MessageEventLike {
  readonly origin: string;
  readonly data: unknown;
  readonly source?: unknown;
}

/** A `StorageEvent`-like object (for the localStorage relay fallback). */
export interface StorageEventLike {
  readonly key: string | null;
  readonly newValue: string | null;
  readonly storageArea?: unknown;
}

/** A `BroadcastChannel`-like handle. */
export interface BroadcastChannelLike {
  onmessage: ((ev: { data: unknown }) => void) | null;
  postMessage(message: unknown): void;
  close(): void;
}

/** A `Storage`-like handle (sessionStorage / localStorage). */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
  key(index: number): string | null;
  readonly length: number;
}

/** Document.cookie accessor (breadcrumb reads/writes). */
export interface CookieJar {
  get(): string;
  set(value: string): void;
}

/** A `Crypto`-like handle exposing `getRandomValues` + `subtle.digest`. */
export interface CryptoLike {
  getRandomValues<T extends ArrayBufferView>(array: T): T;
  subtle: {
    digest(algorithm: "SHA-256", data: ArrayBuffer | ArrayBufferView): Promise<ArrayBuffer>;
  };
}

/** Popup poll/timeout timings (overridable in tests; production defaults otherwise). */
export interface PopupTiming {
  /** Interval (ms) between `popup.closed` polls. */
  pollMs: number;
  /** Hard timeout (ms) after which the popup flow rejects with `timeout`. */
  timeoutMs: number;
}

/** Fully-resolved environment the client modules consume. */
export interface ResolvedEnv {
  window: WindowLike;
  fetch: typeof fetch;
  /** sessionStorage — durable PKCE-transaction store. */
  session: StorageLike;
  /** localStorage — relay-event fallback only (NOT a token store). */
  local: StorageLike;
  location: { origin: string };
  cookies: CookieJar;
  crypto: CryptoLike;
  /** Construct a same-origin BroadcastChannel; undefined ⇒ not supported. */
  broadcastChannel?: (name: string) => BroadcastChannelLike;
  /** Subscribe to cross-tab `storage` events (relay fallback). */
  onStorage?: (listener: (ev: StorageEventLike) => void) => () => void;
  /** Popup poll/timeout timings. */
  popupTiming: PopupTiming;
}

/** Production popup timing: poll closed every 1 s, time out at 60 s (gateway §4.4). */
export const DEFAULT_POPUP_TIMING: PopupTiming = { pollMs: 1000, timeoutMs: 60_000 };

/** The partial environment a caller may inject; missing fields fall to globals. */
export interface ClientEnv {
  window?: WindowLike;
  fetch?: typeof fetch;
  session?: StorageLike;
  local?: StorageLike;
  location?: { origin: string };
  cookies?: CookieJar;
  crypto?: CryptoLike;
  broadcastChannel?: (name: string) => BroadcastChannelLike;
  onStorage?: (listener: (ev: StorageEventLike) => void) => () => void;
  /** Override the popup poll/timeout timings (tests run them short). */
  popupTiming?: Partial<PopupTiming>;
}

/**
 * Resolve a {@link ClientEnv} against the ambient globals. Fields the caller
 * supplied win; everything else is read from `globalThis`. Throwing accessors
 * (e.g. `window` is undefined in Node) are tolerated — the client only fails
 * if a method it actually calls is missing.
 */
export function resolveEnv(env?: ClientEnv): ResolvedEnv {
  const g = globalThis as unknown as {
    window?: WindowLike;
    fetch?: typeof fetch;
    sessionStorage?: StorageLike;
    localStorage?: StorageLike;
    location?: { origin: string };
    document?: { cookie: string };
    crypto?: CryptoLike;
    BroadcastChannel?: new (name: string) => BroadcastChannelLike;
    addEventListener?: (type: string, listener: (ev: unknown) => void) => void;
    removeEventListener?: (type: string, listener: (ev: unknown) => void) => void;
  };

  const defaultCookies: CookieJar = {
    get: () => g.document?.cookie ?? "",
    set: (value) => {
      if (g.document) g.document.cookie = value;
    },
  };

  const broadcastChannel =
    env?.broadcastChannel ??
    (g.BroadcastChannel
      ? (name: string) => new g.BroadcastChannel!(name)
      : undefined);

  const onStorage =
    env?.onStorage ??
    (g.addEventListener && g.removeEventListener
      ? (listener: (ev: StorageEventLike) => void) => {
          const wrapped = (ev: unknown) => listener(ev as StorageEventLike);
          g.addEventListener!("storage", wrapped);
          return () => g.removeEventListener!("storage", wrapped);
        }
      : undefined);

  return {
    window: env?.window ?? (g.window as WindowLike),
    // Bind to the global: the default `fetch` is later invoked as a property
    // (`transport.fetchImpl(...)`), which would call the platform `fetch` with
    // `this` = the Transport instance — browsers reject that with a
    // "TypeError: Illegal invocation". `.bind(g)` pins `this` to the global.
    fetch: env?.fetch ?? ((g.fetch as typeof fetch | undefined)?.bind(g) as typeof fetch),
    session: env?.session ?? (g.sessionStorage as StorageLike),
    local: env?.local ?? (g.localStorage as StorageLike),
    location: env?.location ?? (g.location as { origin: string }),
    cookies: env?.cookies ?? defaultCookies,
    crypto: env?.crypto ?? (g.crypto as CryptoLike),
    broadcastChannel,
    onStorage,
    popupTiming: { ...DEFAULT_POPUP_TIMING, ...env?.popupTiming },
  };
}
