// packages/zeroship-rpc-client/src/transport.ts
//
// Single-request transport for unary procedures (query / mutation).
// Streams and subscriptions are NOT covered here — Phase 4 / Phase 8.
//
// Wire shape:
//
//   query (small)  → GET /_zs/v1/<id>?input=<base64url-superjson>
//   query (>6 KB)  → POST /_zs/v1/<id> with `X-Method: GET` header
//   mutation       → POST /_zs/v1/<id>
//
// Request headers (always set):
//   - Accept: application/json
//   - Content-Type: application/json (only when a body is present)
//   - Authorization: Bearer <token>  (when auth resolves to a string)
//   - Idempotency-Key: <uuidv7>      (mutations w/ idempotent: true)
//   - X-Request-Id: <uuidv7>         (every request — for tracing)

import { decodeBody, encodeBody, encodeQueryInput, type Transformer } from "./encoding.js";
import { parseErrorResponse, RpcError } from "./error.js";
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
      code: "UNIMPLEMENTED",
      message: "stream procedures are not supported in Phase 3 of @zeroship/rpc-client",
      retryable: false,
    });
  }
  if (opts.kind === "subscription") {
    throw new RpcError({
      code: "UNIMPLEMENTED",
      message:
        "subscription procedures are not supported in Phase 3 of @zeroship/rpc-client",
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
