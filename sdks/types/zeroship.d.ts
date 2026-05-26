/**
 * Type stubs for the user-facing `zeroship` ESM module.
 *
 * The runtime synthesizes a virtual module named `"zeroship"` that exports
 * three things the SDK packages consume:
 *
 *   - `env`: the composite handler env (plugin namespaces + app secrets),
 *     same reference as the 2nd arg of `fetch(request, env, ctx)`.
 *   - `waitUntil(p)`: extend the request lifetime past its response.
 *   - `getRequest()`: look up the current Request from nested modules.
 *
 * This declaration lets `import { env } from "zeroship"` resolve during
 * TypeScript build. At runtime, the module is provided by the V8 kernel
 * (see `crates/runtime/src/init.rs::ZEROSHIP_MODULE_JS`).
 *
 * The `env` type is deliberately loose: plugin namespaces are attached at
 * runtime based on which plugins the worker registered, and scalar secrets
 * vary per app. Typed subsurfaces (`ZeroshipDb`, `ZeroshipAuth`) live in the
 * sibling .d.ts files — the `env` entries use them as optional hints.
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />

declare module "zeroship" {
  /**
   * Composite per-request env — same object as the `env` arg of
   * `fetch(request, env, ctx)`. Plugin namespaces appear under their
   * declared keys ("db", "kv", "storage", ...); app-scoped secrets and
   * variables appear as string keys at the top level.
   *
   * Frozen: direct assignment to properties throws in strict mode.
   *
   * The interface is named (vs. a structural literal) so user code can
   * augment `env.db` with collection-typed accessors via the
   * `zeroship-schema` virtual path. That schema-aware augmentation
   * lives in `@zeroship/db`; this package owns only the base runtime
   * module shape.
   */
  export interface Env {
    // `db` and `auth` are populated by their respective augmentations.
    // Keeping them out of the base declaration lets narrower SDK
    // augmentations be the source of truth.
    [key: string]: unknown;
  }
  export const env: Env;

  /**
   * Register a promise the runtime should await before the request's
   * isolate slot is released. Lets fire-and-forget work (log flush,
   * webhook retry) finish past the response body write.
   */
  export function waitUntil(promise: Promise<unknown>): void;

  /**
   * Return the Request currently being handled. Throws if called
   * outside a request (e.g. at module-init time).
   */
  export function getRequest(): Request;

  /**
   * RPC composition primitive — invoke a `query()` procedure. Threads
   * the inner kind onto the capability stack so capability gates
   * (fetch refusal, DB-write refusal) see the inner procedure's kind,
   * not the caller's.
   */
  export function runQuery<TIn, TOut>(
    fn: (input: TIn) => Promise<TOut> | TOut,
    input: TIn,
  ): Promise<TOut>;

  /**
   * RPC composition primitive — invoke a `mutation()` procedure. Each
   * database operation autocommits unless the handler opens an explicit
   * `db.transaction()`.
   */
  export function runMutation<TIn, TOut>(
    fn: (input: TIn) => Promise<TOut> | TOut,
    input: TIn,
  ): Promise<TOut>;

  // ── Per-request accessors ──────────────────────────────────────────
  // Each throws "<name>: called outside a request handler" when no
  // dispatch frame is active.

  /** Authenticated user from the gateway JWT, or `null` if unauthenticated. */
  export function currentUser(): unknown | null;

  /** Per-request request id (e.g. `req_<16 hex>`). */
  export function currentRequestId(): string;

  /** Per-request W3C trace id (32 hex chars). */
  export function currentTraceId(): string;

  /** AbortSignal that fires when the request times out or is cancelled. */
  export function currentSignal(): AbortSignal;

  /** Incoming HTTP headers (mutable, request-scoped). */
  export function currentHeaders(): Headers;

  /** `Idempotency-Key` request header value, or `undefined`. */
  export function currentIdempotencyKey(): string | undefined;
}
