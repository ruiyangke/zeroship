//
// Request transports for unary procedures (query / mutation) and
// streams. WebSocket subscriptions are not implemented in this package
// yet.
//
// Wire shape:
//
//   query (small)  → GET /_zs/v1/<id>?input=<base64url-superjson>
//   query (>6 KB)  → POST /_zs/v1/<id> with `X-Method: GET` header
//   mutation       → POST /_zs/v1/<id>
//   stream         → POST /_zs/v1/<id> with Accept: text/event-stream
//                    response body uses the Vercel AI-SDK Data Stream
//                    Protocol (line-prefixed `<typeId>:<json>\n`).
//
// Request headers (always set):
//   - Accept: application/json | text/event-stream (stream)
//   - Content-Type: application/json (only when a body is present)
//   - Authorization: Bearer <token>  (when auth resolves to a string)
//   - Idempotency-Key: <uuidv7>      (mutations w/ idempotent: true)
//   - X-Request-Id: <uuidv7>         (every request — for tracing)

import { decodeBody, encodeBody, encodeQueryInput, type Transformer } from "./encoding.js";
import { parseErrorResponse, RpcError, type ErrorCode } from "./error.js";
import { newUuidV7 } from "./idempotency.js";

/** URL byte threshold above which queries fall back to POST + X-Method:GET. */
const URL_FALLBACK_BYTES = 6 * 1024;

/** Discriminator for the *kind* a transport call is operating in. */
export type CallKind = "query" | "mutation" | "stream" | "subscription";

export interface TransportOptions {
  /** Procedure kind. Drives HTTP method choice + body encoding. */
  kind: CallKind;
  /** Idempotent? When true and kind is "mutation", we add Idempotency-Key. */
  idempotent?: boolean;
  /** Per-call signal for cancellation. */
  signal?: AbortSignal;
  /** Per-call extra headers (merged after built-ins; user wins). */
  headers?: Record<string, string>;
  /**
   * Per-call timeout in ms. When set, an internal AbortController is
   * triggered and the promise rejects with `code: "TIMEOUT"`. Composes
   * with `signal` — whichever fires first wins.
   */
  timeout?: number;
}

export interface TransportConfig {
  baseUrl: string;
  fetch: typeof globalThis.fetch;
  transformer: Transformer;
  /** Resolves to the bearer token string, or null/undefined to skip the header. */
  authResolver: () => string | null | undefined | Promise<string | null | undefined>;
  /** Optional global hooks. */
  onError?: (err: RpcError) => void;
  onAuthExpired?: () => void;
}

/**
 * Send a single unary RPC and decode the response. Throws RpcError on
 * any failure (4xx/5xx, transport error, abort).
 */
export async function sendUnary<TOut = unknown>(
  procId: string,
  input: unknown,
  cfg: TransportConfig,
  opts: TransportOptions,
): Promise<TOut> {
  if (opts.kind === "stream") {
    throw new RpcError({
      code: "INTERNAL",
      message:
        "[zeroship/rpc-client] streamCall must handle 'stream' kind, not sendUnary",
      retryable: false,
    });
  }
  if (opts.kind === "subscription") {
    throw new RpcError({
      code: "UNIMPLEMENTED",
      message:
        "subscription procedures are not supported by @zeroship/rpc-client yet",
      retryable: false,
    });
  }

  // Resolve auth before assembling headers (the resolver may throw, in
  // which case we surface the failure verbatim — better than masking it
  // as INTERNAL).
  let authToken: string | null | undefined;
  try {
    authToken = await cfg.authResolver();
  } catch (e) {
    // Re-throw the resolver's own error; the user installed it for a
    // reason and we don't second-guess.
    throw e instanceof Error ? e : new Error(String(e));
  }

  const headers = new Headers();
  headers.set("Accept", "application/json");
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
  headers.set("X-Request-Id", newUuidV7());

  let url: string;
  let method: string;
  let body: string | undefined;

  if (opts.kind === "query") {
    if (input === undefined) {
      url = `${cfg.baseUrl}/_zs/v1/${procId}`;
      method = "GET";
    } else {
      const enc = await encodeQueryInput(input, cfg.transformer);
      const candidate = `${cfg.baseUrl}/_zs/v1/${procId}?input=${enc}`;
      if (byteLength(candidate) > URL_FALLBACK_BYTES) {
        // Fallback: POST with body, signal "still a query" via header.
        url = `${cfg.baseUrl}/_zs/v1/${procId}`;
        method = "POST";
        body = await encodeBody(input, cfg.transformer);
        headers.set("Content-Type", "application/json");
        headers.set("X-Method", "GET");
      } else {
        url = candidate;
        method = "GET";
      }
    }
  } else {
    // Mutation.
    url = `${cfg.baseUrl}/_zs/v1/${procId}`;
    method = "POST";
    body = await encodeBody(input, cfg.transformer);
    headers.set("Content-Type", "application/json");
    if (opts.idempotent) {
      headers.set("Idempotency-Key", newUuidV7());
    }
  }

  // Per-call header overrides (user wins).
  if (opts.headers) {
    for (const [k, v] of Object.entries(opts.headers)) {
      headers.set(k, v);
    }
  }

  // Compose abort signals (caller signal + timeout signal).
  const callerSignal = opts.signal;
  const timeoutCtrl = opts.timeout && opts.timeout > 0 ? new AbortController() : null;
  const timeoutHandle = timeoutCtrl
    ? setTimeout(() => timeoutCtrl.abort(), opts.timeout!)
    : null;
  const signal = composeSignals(callerSignal, timeoutCtrl?.signal);

  let res: Response;
  try {
    res = await cfg.fetch(url, {
      method,
      headers,
      body,
      signal,
    });
  } catch (err) {
    if (timeoutHandle) clearTimeout(timeoutHandle);
    // Abort + timeout categorization.
    if (isAbortError(err)) {
      const timedOut = timeoutCtrl?.signal.aborted ?? false;
      const rpcErr = new RpcError({
        code: timedOut ? "TIMEOUT" : "CANCELLED",
        message: timedOut ? "request timed out" : "request cancelled",
        retryable: timedOut,
      });
      cfg.onError?.(rpcErr);
      throw rpcErr;
    }
    // Generic transport failure → UNAVAILABLE.
    const message = err instanceof Error ? err.message : String(err);
    const rpcErr = new RpcError({
      code: "UNAVAILABLE",
      message: `transport error: ${message}`,
      retryable: true,
    });
    cfg.onError?.(rpcErr);
    throw rpcErr;
  }
  if (timeoutHandle) clearTimeout(timeoutHandle);

  if (!res.ok) {
    const err = await parseErrorResponse(res);
    if (err.code === "UNAUTHENTICATED" && cfg.onAuthExpired) {
      cfg.onAuthExpired();
    }
    cfg.onError?.(err);
    throw err;
  }

  const text = await res.text();
  return decodeBody<TOut>(text, cfg.transformer);
}

function isAbortError(err: unknown): boolean {
  if (!err || typeof err !== "object") return false;
  const e = err as { name?: string; code?: string };
  return e.name === "AbortError" || e.code === "ABORT_ERR";
}

// ── Streams ─────────────────────────────────────────────────────────────

/** Per-call options for `streamCall`. Same shape as `TransportOptions` minus `kind`. */
export interface StreamOptions {
  signal?: AbortSignal;
  headers?: Record<string, string>;
  timeout?: number;
}

/**
 * POST /_zs/v1/<id> with `Accept: text/event-stream` and read the
 * response body as a Vercel AI-SDK Data Stream.
 *
 * Returned async-iter yields each value (parsed per the typeId rule):
 *
 *   `0:"text"` → yield `"text"` (string)
 *   `2:[<json>]` → yield each element of the array (objects)
 *   `3:"err"` → throw RpcError(INTERNAL, "err")
 *   `e:{...}` → throw RpcError using the structured envelope
 *   `d:{}` → end iteration
 *
 * Iteration is fully demand-driven: the underlying ReadableStream
 * reader is only advanced when the consumer asks for the next value.
 */
export function streamCall<TOut = unknown>(
  procId: string,
  input: unknown,
  cfg: TransportConfig,
  opts: StreamOptions = {},
): AsyncIterableIterator<TOut> {
  // Lazily kick off the request when iteration starts. We can't make
  // it eager — the consumer may never iterate, in which case we'd
  // leak an open connection.
  let requestStarted = false;
  let requestPromise: Promise<{ reader: ReadableStreamDefaultReader<Uint8Array> }> | null = null;
  let pending: TOut[] = [];
  let buffer = "";
  let done = false;
  let thrown: unknown = null;
  const decoder = new TextDecoder();

  // Compose abort signals.
  const callerSignal = opts.signal;
  const timeoutCtrl = opts.timeout && opts.timeout > 0 ? new AbortController() : null;
  const timeoutHandle = timeoutCtrl
    ? setTimeout(() => timeoutCtrl.abort(), opts.timeout!)
    : null;
  const signal = composeSignals(callerSignal, timeoutCtrl?.signal);

  function clearTimeoutOnce(): void {
    if (timeoutHandle) clearTimeout(timeoutHandle);
  }

  async function startRequest(): Promise<{ reader: ReadableStreamDefaultReader<Uint8Array> }> {
    let authToken: string | null | undefined;
    try {
      authToken = await cfg.authResolver();
    } catch (e) {
      throw e instanceof Error ? e : new Error(String(e));
    }

    const headers = new Headers();
    headers.set("Accept", "text/event-stream");
    headers.set("Content-Type", "application/json");
    if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
    headers.set("X-Request-Id", newUuidV7());
    if (opts.headers) {
      for (const [k, v] of Object.entries(opts.headers)) headers.set(k, v);
    }

    const url = `${cfg.baseUrl}/_zs/v1/${procId}`;
    const body = await encodeBody(input, cfg.transformer);

    let res: Response;
    try {
      res = await cfg.fetch(url, { method: "POST", headers, body, signal });
    } catch (err) {
      clearTimeoutOnce();
      if (isAbortError(err)) {
        const timedOut = timeoutCtrl?.signal.aborted ?? false;
        const rpcErr = new RpcError({
          code: timedOut ? "TIMEOUT" : "CANCELLED",
          message: timedOut ? "request timed out" : "request cancelled",
          retryable: timedOut,
        });
        cfg.onError?.(rpcErr);
        throw rpcErr;
      }
      const message = err instanceof Error ? err.message : String(err);
      const rpcErr = new RpcError({
        code: "UNAVAILABLE",
        message: `transport error: ${message}`,
        retryable: true,
      });
      cfg.onError?.(rpcErr);
      throw rpcErr;
    }

    if (!res.ok) {
      clearTimeoutOnce();
      const err = await parseErrorResponse(res);
      if (err.code === "UNAUTHENTICATED" && cfg.onAuthExpired) cfg.onAuthExpired();
      cfg.onError?.(err);
      throw err;
    }
    if (!res.body) {
      clearTimeoutOnce();
      const rpcErr = new RpcError({
        code: "INTERNAL",
        message: "stream response had no body",
        retryable: false,
      });
      cfg.onError?.(rpcErr);
      throw rpcErr;
    }
    return { reader: res.body.getReader() };
  }

  function consumeLine(line: string): boolean {
    // Parse a single AI-SDK Data Stream frame. Returns true when the
    // stream is done (`d:` typeId); false to keep reading.
    if (line.length === 0) return false;
    const colonIdx = line.indexOf(":");
    if (colonIdx <= 0) return false;
    const typeId = line.slice(0, colonIdx);
    const json = line.slice(colonIdx + 1);
    switch (typeId) {
      case "0": {
        // `0:"text"` — JSON-stringified string.
        let v: string;
        try {
          v = JSON.parse(json);
        } catch {
          return false;
        }
        pending.push(v as TOut);
        return false;
      }
      case "2": {
        // `2:[<json>]` — array, yield each element.
        let arr: unknown;
        try {
          arr = JSON.parse(json);
        } catch {
          return false;
        }
        if (Array.isArray(arr)) {
          for (const el of arr) pending.push(el as TOut);
        }
        return false;
      }
      case "3": {
        // `3:"err"` — string-only error message.
        let msg: string;
        try {
          msg = JSON.parse(json);
        } catch {
          msg = json;
        }
        thrown = new RpcError({
          code: "INTERNAL",
          message: msg,
          retryable: false,
        });
        return true;
      }
      case "e": {
        // `e:{...}` — structured envelope. Lift code/message/details/retryable.
        let env: Record<string, unknown>;
        try {
          env = JSON.parse(json) as Record<string, unknown>;
        } catch {
          thrown = new RpcError({
            code: "INTERNAL",
            message: "invalid error envelope",
            retryable: false,
          });
          return true;
        }
        const code = (typeof env.code === "string" ? (env.code as ErrorCode) : "INTERNAL");
        const message = typeof env.message === "string" ? env.message : "stream error";
        thrown = new RpcError({
          code,
          message,
          details: env.details,
          retryable: typeof env.retryable === "boolean" ? env.retryable : undefined,
        });
        return true;
      }
      case "d": {
        // `d:{}` — done.
        return true;
      }
      default:
        // Unknown typeId — ignore (forward-compat with future ai-sdk
        // protocol additions).
        return false;
    }
  }

  async function pump(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<void> {
    // Read at most one chunk; transfer parsed lines into `pending`.
    // Loops only when a chunk produced no lines (partial line buffered)
    // — otherwise we return to give the consumer a chance to drain.
    while (true) {
      let read: ReadableStreamReadResult<Uint8Array>;
      try {
        read = await reader.read();
      } catch (err) {
        if (isAbortError(err)) {
          const timedOut = timeoutCtrl?.signal.aborted ?? false;
          thrown = new RpcError({
            code: timedOut ? "TIMEOUT" : "CANCELLED",
            message: timedOut ? "request timed out" : "request cancelled",
            retryable: timedOut,
          });
        } else {
          const message = err instanceof Error ? err.message : String(err);
          thrown = new RpcError({
            code: "UNAVAILABLE",
            message: `transport error: ${message}`,
            retryable: true,
          });
        }
        done = true;
        return;
      }
      if (read.done) {
        // EOF without `d:` is OK — treat as clean stream end.
        // Flush any remaining buffered partial line.
        if (buffer.length > 0) {
          const isDone = consumeLine(buffer);
          buffer = "";
          if (isDone) done = true;
        }
        done = true;
        return;
      }
      buffer += decoder.decode(read.value, { stream: true });
      let nl: number;
      let producedLine = false;
      while ((nl = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        producedLine = true;
        const isDone = consumeLine(line);
        if (isDone) {
          done = true;
          return;
        }
      }
      // If we produced at least one line, return — let consumer drain.
      if (producedLine) return;
      // No line yet (partial); loop and read more.
    }
  }

  let activeReader: ReadableStreamDefaultReader<Uint8Array> | null = null;

  async function nextValue(): Promise<IteratorResult<TOut>> {
    // First call: start the request. Errors here surface as the
    // iterator throwing, matching `for await` semantics.
    if (!requestStarted) {
      requestStarted = true;
      requestPromise = startRequest();
    }
    if (activeReader === null) {
      try {
        const { reader } = await requestPromise!;
        activeReader = reader;
      } catch (err) {
        clearTimeoutOnce();
        throw err;
      }
    }
    while (pending.length === 0 && !done && thrown === null) {
      await pump(activeReader);
    }
    if (pending.length > 0) {
      const v = pending.shift()!;
      return { value: v, done: false };
    }
    clearTimeoutOnce();
    if (thrown !== null) {
      const err = thrown;
      thrown = null;
      cfg.onError?.(err as RpcError);
      throw err;
    }
    return { value: undefined as unknown as TOut, done: true };
  }

  const iter: AsyncIterableIterator<TOut> = {
    next: nextValue,
    async return(value?: TOut): Promise<IteratorResult<TOut>> {
      // Consumer broke out of the loop early. Cancel the upstream
      // reader so the connection closes cleanly.
      clearTimeoutOnce();
      try {
        if (activeReader) await activeReader.cancel();
      } catch {
        // ignore — best effort.
      }
      done = true;
      return { value: value as TOut, done: true };
    },
    async throw(err?: unknown): Promise<IteratorResult<TOut>> {
      clearTimeoutOnce();
      try {
        if (activeReader) await activeReader.cancel();
      } catch {
        // ignore.
      }
      done = true;
      throw err;
    },
    [Symbol.asyncIterator](): AsyncIterableIterator<TOut> {
      return iter;
    },
  };
  return iter;
}

// ── Subscriptions ────────────────────────────────────────────────────────
//
// Wire — see `docs/proposals/rpc-v2.md` §6 (Subscription wire) and the
// runtime's `_zsAcceptSubscription` (`crates/runtime/src/init.rs`).
//
//   GET wss://.../_zs/v1/<id>
//   Sec-WebSocket-Protocol: zs.v1
//   Authorization: Bearer <jwt>      (optional — cookie auth also OK)
//
// Frame protocol (text JSON, one message per frame):
//
//   client → server (FIRST):   {"t":"hello","input":<json>}
//   server → client:           {"t":"data","value":<json>}    each yield
//                              {"t":"end"}                    normal completion
//                              {"t":"error","error":<env>}    on throw
//                              {"t":"ping"}                   keepalive
//   either → either:           {"t":"pong"}                   ack to a ping

/** Per-call options for `subscribeCall` (and `proc.subscribe`). */
export interface SubscribeOptions<TOut = unknown> {
  /** Callback invoked for each `{"t":"data"}` frame. */
  onData?: (value: TOut) => void;
  /**
   * Called once on a structured `{"t":"error"}` frame OR when the
   * connection closes abnormally and we couldn't reconnect. Receives an
   * `RpcError`. After `onError` fires, the subscription is dead — no
   * further callbacks fire.
   */
  onError?: (err: RpcError) => void;
  /** Called on `{"t":"end"}` (normal stream completion). */
  onEnd?: () => void;
  /**
   * Called on every successful (re)connection — fires once per
   * `hello` send. Useful for clearing "reconnecting…" UI state.
   */
  onOpen?: () => void;
  /** Per-call abort signal; aborting closes the connection. */
  signal?: AbortSignal;
  /**
   * Disable the auto-reconnect-with-backoff path. Default false (we
   * always retry on abnormal close). When true the subscription
   * surfaces every 1006 close as `onError("ABORTED")` and stops.
   */
  noReconnect?: boolean;
  /**
   * Override the default reconnect backoff cap. Useful for tests
   * (low cap so a flapping mock server settles quickly).
   */
  maxReconnectMs?: number;
}

/** Handle returned by `subscribeCall`. */
export interface SubscriptionHandle {
  /** Close the connection and stop firing callbacks. Idempotent. */
  unsubscribe(): void;
  /**
   * Returns the underlying WebSocket's readyState as a human string
   * (`"connecting" | "open" | "closing" | "closed"`). Mostly for
   * diagnostics + tests.
   */
  readyState: () => "connecting" | "open" | "closing" | "closed" | "idle";
}

const DEFAULT_RECONNECT_CAP_MS = 30_000;

/** Convert a numeric WebSocket readyState to its symbolic name. */
function wsReadyStateName(
  ws: WebSocket | null,
): "connecting" | "open" | "closing" | "closed" | "idle" {
  if (!ws) return "idle";
  switch (ws.readyState) {
    case 0:
      return "connecting";
    case 1:
      return "open";
    case 2:
      return "closing";
    default:
      return "closed";
  }
}

/**
 * Open a WebSocket-backed subscription. Sends `hello` on connect, dispatches
 * incoming frames to the appropriate callbacks, replies to pings with pongs,
 * auto-reconnects with exponential backoff on abnormal closure (1006).
 *
 * Callers receive a `SubscriptionHandle` whose `unsubscribe()` closes
 * cleanly; passing a `signal` does the same on `abort()`.
 *
 * `wsFactory` defaults to `globalThis.WebSocket`. Tests inject a mock.
 */
export function subscribeCall<TOut = unknown>(
  procId: string,
  input: unknown,
  cfg: TransportConfig,
  opts: SubscribeOptions<TOut> = {},
  wsFactory?: (url: string, protocols?: string | string[]) => WebSocket,
): SubscriptionHandle {
  const factory =
    wsFactory ??
    ((url: string, protocols?: string | string[]) =>
      new (globalThis as unknown as { WebSocket: new (u: string, p?: string | string[]) => WebSocket })
        .WebSocket(url, protocols));
  if (!factory) {
    throw new RpcError({
      code: "UNAVAILABLE",
      message:
        "[zeroship/rpc-client] no WebSocket implementation — pass a `wsFactory` or run on a runtime that exposes globalThis.WebSocket",
      retryable: false,
    });
  }

  const wsUrl = buildSubscriptionUrl(procId, cfg.baseUrl);
  const cap = opts.maxReconnectMs ?? DEFAULT_RECONNECT_CAP_MS;

  let ws: WebSocket | null = null;
  let unsubscribed = false;
  let endedNormally = false;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let attempt = 0; // zero-indexed retry counter
  let onceFiredError = false;
  let helloPayload: { json: string } | null = null;

  function clearReconnectTimer(): void {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function fireError(err: RpcError): void {
    if (onceFiredError) return;
    onceFiredError = true;
    cfg.onError?.(err);
    opts.onError?.(err);
  }

  function teardown(reason?: { close?: boolean; code?: number }): void {
    clearReconnectTimer();
    if (ws && reason?.close && (ws.readyState === 0 || ws.readyState === 1)) {
      try {
        ws.close(reason.code ?? 1000, "");
      } catch {
        // ignore — best effort.
      }
    }
    ws = null;
  }

  // ── Connect ────────────────────────────────────────────────────────
  async function connect(): Promise<void> {
    if (unsubscribed) return;
    let authToken: string | null | undefined;
    try {
      authToken = await cfg.authResolver();
    } catch (e) {
      fireError(
        new RpcError({
          code: "UNAUTHENTICATED",
          message: e instanceof Error ? e.message : String(e),
          retryable: false,
        }),
      );
      teardown();
      return;
    }
    if (!helloPayload) {
      const body = await encodeBody(input, cfg.transformer);
      helloPayload = { json: body };
    }

    // The auth header on a WebSocket upgrade isn't reachable from
    // the browser `WebSocket` constructor — only protocols + URL +
    // cookies. We pass the bearer token via a `auth.zsbearer.<token>`
    // sub-protocol token when present (the gateway adapts), and rely
    // on cookie auth otherwise. Same-origin deployments use cookie
    // auth by default; cross-origin deployments inject the JWT into
    // the protocol list so the gateway can lift it without a custom
    // header. (Servers MAY ignore the bearer subprotocol and still
    // authenticate via cookie.)
    const protocols: string[] = ["zs.v1"];
    if (authToken) {
      protocols.push(`auth.zsbearer.${encodeURIComponent(authToken)}`);
    }

    let socket: WebSocket;
    try {
      socket = factory(wsUrl, protocols);
    } catch (e) {
      fireError(
        new RpcError({
          code: "UNAVAILABLE",
          message: `WebSocket open failed: ${
            e instanceof Error ? e.message : String(e)
          }`,
          retryable: true,
        }),
      );
      teardown();
      return;
    }
    ws = socket;

    socket.addEventListener("open", () => {
      if (unsubscribed) {
        try {
          socket.close(1000, "");
        } catch {
          // ignore.
        }
        return;
      }
      attempt = 0;
      // Re-parse the encoded input so the wire carries the wrapped
      // superjson `{ json, meta? }` shape, not a string. Undefined
      // input is omitted from the frame (JSON.stringify drops it),
      // matching `{ t: "hello" }` on the wire.
      const helloFrame: { t: "hello"; input?: unknown } = { t: "hello" };
      if (helloPayload && typeof helloPayload.json === "string") {
        helloFrame.input = JSON.parse(helloPayload.json);
      }
      const helloFrameStr = JSON.stringify(helloFrame);
      try {
        socket.send(helloFrameStr);
      } catch (e) {
        fireError(
          new RpcError({
            code: "UNAVAILABLE",
            message: `failed to send hello: ${
              e instanceof Error ? e.message : String(e)
            }`,
            retryable: true,
          }),
        );
        teardown({ close: true, code: 1011 });
        return;
      }
      opts.onOpen?.();
    });

    socket.addEventListener("message", (ev) => {
      if (unsubscribed) return;
      const data =
        typeof (ev as MessageEvent).data === "string"
          ? ((ev as MessageEvent).data as string)
          : String((ev as MessageEvent).data);
      let msg: { t?: string; value?: unknown; error?: unknown };
      try {
        msg = JSON.parse(data);
      } catch {
        // Drop unparseable frames silently — forward-compat.
        return;
      }
      switch (msg.t) {
        case "data":
          opts.onData?.(msg.value as TOut);
          return;
        case "end":
          endedNormally = true;
          opts.onEnd?.();
          try {
            socket.close(1000, "");
          } catch {
            // ignore.
          }
          return;
        case "error": {
          const env = (msg.error ?? {}) as Record<string, unknown>;
          const code = (typeof env.code === "string" ? env.code : "INTERNAL") as ErrorCode;
          const message = typeof env.message === "string" ? env.message : "subscription error";
          fireError(
            new RpcError({
              code,
              message,
              details: env.details,
              retryable: typeof env.retryable === "boolean" ? env.retryable : false,
            }),
          );
          // Server is about to close; we'll catch the close event
          // and skip reconnect (onceFiredError gates it).
          return;
        }
        case "ping":
          try {
            socket.send(JSON.stringify({ t: "pong" }));
          } catch {
            // ignore.
          }
          return;
        case "pong":
          // We don't currently send ping from the client side; eat
          // any unexpected pongs without complaint.
          return;
        default:
          // Forward-compat: unknown frame tag → ignore.
          return;
      }
    });

    socket.addEventListener("close", (ev) => {
      const closeEvent = ev as CloseEvent;
      const code = closeEvent.code ?? 1006;
      // Treat 1000 and "we already saw an error" as terminal —
      // explicit close, don't retry.
      if (unsubscribed || endedNormally || onceFiredError) {
        ws = null;
        return;
      }
      // 1006: abnormal closure (e.g. server crashed, network blip).
      // 4xxx: protocol violations from the server (don't retry).
      if (code === 1006 && !opts.noReconnect) {
        scheduleReconnect();
      } else {
        fireError(
          new RpcError({
            code: "ABORTED",
            message: `subscription closed (code ${code})`,
            retryable: false,
          }),
        );
        teardown();
      }
    });

    socket.addEventListener("error", () => {
      // The "error" event always precedes "close" with a 1006 code in
      // browsers; we let "close" handle the reconnect / onError
      // decision. Don't fire onError here or we'll double-fire.
    });
  }

  // ── Reconnect ──────────────────────────────────────────────────────
  function scheduleReconnect(): void {
    if (unsubscribed) return;
    clearReconnectTimer();
    // Exponential backoff with jitter, capped at `cap` ms.
    const base = Math.min(cap, 100 * Math.pow(2, attempt));
    const jitter = Math.random() * (base / 2);
    const delay = Math.floor(base + jitter);
    attempt += 1;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect().catch((e) => {
        fireError(
          e instanceof RpcError
            ? e
            : new RpcError({
                code: "UNAVAILABLE",
                message: e instanceof Error ? e.message : String(e),
                retryable: true,
              }),
        );
        teardown();
      });
    }, delay);
  }

  // ── Wire signal ──
  if (opts.signal) {
    if (opts.signal.aborted) {
      // Abort fired before we even started — fire CANCELLED and bail.
      fireError(
        new RpcError({ code: "CANCELLED", message: "aborted", retryable: false }),
      );
      return {
        unsubscribe: () => {
          unsubscribed = true;
        },
        readyState: () => "idle",
      };
    }
    opts.signal.addEventListener(
      "abort",
      () => {
        unsubscribed = true;
        teardown({ close: true, code: 1000 });
      },
      { once: true },
    );
  }

  // Kick off the first connect on the next tick so the caller can
  // attach its own listeners synchronously after `subscribe()` returns.
  Promise.resolve().then(() => {
    connect().catch((e) => {
      fireError(
        e instanceof RpcError
          ? e
          : new RpcError({
              code: "UNAVAILABLE",
              message: e instanceof Error ? e.message : String(e),
              retryable: true,
            }),
      );
      teardown();
    });
  });

  return {
    unsubscribe: () => {
      if (unsubscribed) return;
      unsubscribed = true;
      teardown({ close: true, code: 1000 });
    },
    readyState: () => wsReadyStateName(ws),
  };
}

/**
 * Build the subscription WebSocket URL from the procedure id +
 * baseUrl. http(s) → ws(s); empty / relative baseUrl is honored
 * verbatim (browsers resolve it against `location`).
 */
export function buildSubscriptionUrl(procId: string, baseUrl: string): string {
  const path = `/_zs/v1/${procId}`;
  if (!baseUrl) return path; // relative — browser resolves against location.
  if (baseUrl.startsWith("http://")) return `ws://${baseUrl.slice(7)}${path}`;
  if (baseUrl.startsWith("https://")) return `wss://${baseUrl.slice(8)}${path}`;
  // Already ws / wss / relative-ish — concat.
  return `${baseUrl}${path}`;
}

/**
 * Build the streaming URL for a procedure. Used by consumers handing
 * the URL to ai-sdk's `useChat({ api: rpc.chat.completion.streamUrl(input) })`.
 *
 * Wire shape matches `streamCall` (POST + Accept: text/event-stream),
 * but ai-sdk's `useChat` only takes a URL — it builds the body itself.
 * For the input-via-query-string compat case we expose the query-string
 * form as well: `?input=<base64url>`.
 */
export function buildStreamUrl(
  procId: string,
  input: unknown,
  cfg: TransportConfig,
): Promise<string> | string {
  if (input === undefined) {
    return `${cfg.baseUrl}/_zs/v1/${procId}`;
  }
  return encodeQueryInput(input, cfg.transformer).then(
    (enc) => `${cfg.baseUrl}/_zs/v1/${procId}?input=${enc}`,
  );
}

function byteLength(str: string): number {
  if (typeof Buffer !== "undefined") return Buffer.byteLength(str, "utf8");
  return new TextEncoder().encode(str).byteLength;
}

/**
 * Compose two AbortSignals into one. Native AbortSignal.any exists in
 * modern runtimes; we polyfill via an internal AbortController for
 * older targets so the client works everywhere fetch does.
 */
function composeSignals(
  a: AbortSignal | undefined,
  b: AbortSignal | undefined,
): AbortSignal | undefined {
  if (!a) return b;
  if (!b) return a;
  // Prefer native AbortSignal.any if available.
  const anyFn = (AbortSignal as unknown as { any?: (s: AbortSignal[]) => AbortSignal })
    .any;
  if (typeof anyFn === "function") return anyFn([a, b]);
  // Manual merge.
  const ctrl = new AbortController();
  const onA = () => ctrl.abort(a.reason);
  const onB = () => ctrl.abort(b.reason);
  if (a.aborted) ctrl.abort(a.reason);
  else a.addEventListener("abort", onA, { once: true });
  if (b.aborted) ctrl.abort(b.reason);
  else b.addEventListener("abort", onB, { once: true });
  return ctrl.signal;
}
