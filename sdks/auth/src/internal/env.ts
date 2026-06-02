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

/**
 * The cross-origin login `<iframe>` element handle (only the bits the iframe
 * driver touches). Its `src` is set on the ELEMENT — NEVER via
 * `contentWindow.location.href`, which throws `SecurityError` cross-origin.
 * `remove()` is the iframe analogue of `popup.close()`: it detaches the element
 * from the document, killing the framed navigation.
 */
export interface IframeLike {
  /** The element `src` attribute — set to the authorize URL to navigate the frame. */
  src: string;
  /** Detach the iframe element from the document (teardown / cancel). */
  remove(): void;
  /**
   * Optional user-cancel signal — an iframe has no `.closed` event (unlike a
   * popup), so the host modal's close affordance resolves this to reject the
   * relay race with `popup_closed` (§8). Absent ⇒ no in-flow user-cancel (only
   * the relay or the 60 s timeout settles `runIframe`).
   */
  readonly cancelled?: Promise<void>;
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

/** The minimal `Element` the default iframe factory mounts/sets `src` on. */
interface DomElementLike {
  src: string;
  setAttribute(name: string, value: string): void;
  remove(): void;
}

/**
 * A mount host the iframe is appended INTO (the modal's host slot). The factory
 * only ever appends the iframe element it just created; the parameter is `any`
 * so a real DOM `Element` (whose `appendChild<T extends Node>` signature is not
 * structurally assignable to a narrowed `DomElementLike` parameter) satisfies
 * this — the React `AuthModal` passes its host `<div>` ref directly.
 */
interface MountHostLike {
  // `unknown` param (method syntax → bivariant) keeps a real DOM `Element` —
  // whose `appendChild<T extends Node>(node: T)` is not assignable to a narrowed
  // `DomElementLike` parameter — structurally compatible. The factory only ever
  // appends the iframe element it just created.
  appendChild(el: unknown): void;
}

/** The minimal `Document` surface the default iframe factory + cookie jar read. */
interface DocumentLike {
  cookie: string;
  createElement(tag: "iframe"): DomElementLike;
  body?: MountHostLike | null;
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
  /**
   * Create + mount the cross-origin login `<iframe>` with its `src` PRE-SET to
   * the authorize URL (or assignable on the returned element). Injectable so the
   * iframe driver is unit-testable against a fake exactly like {@link WindowLike.open}.
   * `undefined` ⇒ no DOM (e.g. SSR / Node) → the launcher falls back to popup.
   */
  createIframe?: (url: string) => IframeLike;
  /**
   * Resolve the DOM host the default {@link createIframe} mounts the iframe
   * INTO (the modal's host slot). Returning a non-null element makes the iframe
   * fill that slot (`width/height:100%`) so the modal's own chrome — title,
   * accessible close button — stays ABOVE it; returning `null`/`undefined`
   * falls back to the full-viewport overlay on `document.body` (the headless,
   * modal-less default). Set by the React `AuthModal` to its host-slot ref
   * (§4.1, §8/§10.5: the modal close affordance must be reachable).
   */
  iframeMount?: () => MountHostLike | null;
  /**
   * Resolve the per-flow USER-CANCEL promise the default {@link createIframe}
   * attaches to the returned {@link IframeLike.cancelled}. The host modal
   * resolves it when the user dismisses the modal, so `runIframe` rejects
   * `popup_closed` and tears the frame down (§8). `undefined` ⇒ no in-flow
   * user-cancel (only the relay or the 60 s timeout settles the flow).
   */
  iframeCancelled?: () => Promise<void> | undefined;
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
  /** Inject a fake iframe factory (tests) — the iframe analogue of `window.open`. */
  createIframe?: (url: string) => IframeLike;
  /** Resolve the host slot the default iframe factory mounts into (AuthModal). */
  iframeMount?: () => MountHostLike | null;
  /** Resolve the per-flow user-cancel promise wired to `IframeLike.cancelled`. */
  iframeCancelled?: () => Promise<void> | undefined;
}

/**
 * The registrable-domain (eTLD+1) of an origin, for the same-site iframe gate.
 *
 * This is a deliberately small, dependency-free heuristic — NOT a full Public
 * Suffix List. It returns the last two dot-labels of the host (`a.b.zeroship.ai`
 * → `zeroship.ai`); single-label hosts (`localhost`) and IPs return as-is. This
 * is sufficient for the gate's job: it is a fail-safe UX selector, never a
 * security boundary (the browser-enforced `frame-ancestors` allowlist is the
 * actual gate, §6.5). A wrong answer only costs a fallback to the working popup.
 */
function registrableDomain(origin: string): string | null {
  let host: string;
  try {
    host = new URL(origin).hostname;
  } catch {
    return null;
  }
  if (!host) return null;
  // IPv4 / bracketed IPv6 — compare whole-host.
  if (/^\d+\.\d+\.\d+\.\d+$/.test(host) || host.startsWith("[")) return host;
  const labels = host.split(".");
  if (labels.length <= 2) return host;
  return labels.slice(-2).join(".");
}

/**
 * True when `a` and `b` are the SAME SITE (share an eTLD+1) — the gate for the
 * immersive iframe. `console.zeroship.ai` and `auth.zeroship.ai` → same site;
 * `console.acme.com` and `auth.zeroship.ai` → not. An unparseable origin on
 * either side returns `false` (fail-safe → popup).
 */
export function sameSite(a: string, b: string): boolean {
  const da = registrableDomain(a);
  const db = registrableDomain(b);
  return da !== null && da === db;
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
    document?: DocumentLike;
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

  // Default iframe factory: create an `<iframe>` with `src` PRE-SET to the
  // authorize URL (never assign `contentWindow.location.href`, §4.1). WHERE it
  // mounts depends on `iframeMount`:
  //   • host slot present (the React `AuthModal` wired its ref) → mount INSIDE
  //     the slot, filling it (`width/height:100%`), so the modal's own chrome
  //     (title + accessible close button) stays ABOVE the frame and the cancel
  //     affordance is reachable (§8/§10.5). NO `position:fixed`, NO max z-index.
  //   • no host slot (headless / modal-less) → the bare full-viewport overlay
  //     on `document.body` (the original fallback).
  // Undefined with no DOM (SSR/Node) → the launcher falls back to popup.
  const doc = g.document;
  const iframeMount = env?.iframeMount;
  const iframeCancelled = env?.iframeCancelled;
  const createIframe =
    env?.createIframe ??
    (doc && typeof doc.createElement === "function"
      ? (url: string): IframeLike => {
          const el = doc.createElement("iframe");
          // Set the element `src` (allowed cross-origin) — the load-bearing
          // navigation contract from §4.1.
          el.src = url;
          el.setAttribute("title", "Sign in");
          const host = iframeMount?.() ?? null;
          if (host) {
            // In-slot: fill the host, no overlay positioning so the modal
            // chrome (incl. the close button) is not painted over (§8).
            el.setAttribute(
              "style",
              "width:100%;height:100%;border:0;background:#fff",
            );
            host.appendChild(el);
          } else {
            // Headless fallback: a bare full-viewport overlay.
            el.setAttribute(
              "style",
              "position:fixed;inset:0;width:100%;height:100%;border:0;z-index:2147483647;background:#fff",
            );
            doc.body?.appendChild(el);
          }
          // Snapshot the per-flow user-cancel promise (the modal close
          // affordance, §8) so `runIframe` can reject `popup_closed` on dismiss.
          const cancelled = iframeCancelled?.();
          return {
            get src() {
              return el.src;
            },
            set src(v: string) {
              el.src = v;
            },
            remove() {
              el.remove();
            },
            cancelled,
          };
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
    createIframe,
    iframeMount,
    iframeCancelled,
  };
}
