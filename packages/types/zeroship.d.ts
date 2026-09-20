/**
 * Type stubs for the user-facing `zeroship` ESM module.
 *
 * The runtime supplies a native module named `"zeroship"`. SDK packages use
 * its environment, request helpers and procedure composition exports:
 *
 *   - `env`: the composite handler env (plugin namespaces + app secrets),
 *     same reference as the 2nd arg of `fetch(request, env, ctx)`.
 *   - `waitUntil(p)`: extend the request lifetime past its response.
 *   - `getRequest()`: look up the current Request from nested modules.
 *   - `runQuery` / `runMutation`: invoke a procedure under its native kind.
 *
 * This declaration lets `import { env } from "zeroship"` resolve during
 * TypeScript build. At runtime, the module is provided by the V8 kernel
 * (see `crates/zeroship-runtime/src/core/zeroship_module.rs`).
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
   * Every database this app declares, by its LOCAL LABEL — the key of a
   * `databases` entry in `zeroship.jsonc`, which is also the member name the
   * runtime publishes the handle under.
   *
   * IT IS A SEPARATE INTERFACE, AND THAT IS THE WHOLE DESIGN. Each database
   * gets its own generated `env.db.ts`, and each one augments THIS interface
   * with ITS label. A `databases` property declared inline on `Env` could not
   * work: two generated modules would each declare the same property with a
   * different shape, and declaration merging calls that a conflict, not a
   * union. One interface with one property per label merges; one property
   * redeclared per database does not.
   *
   * EMPTY BY DESIGN, with no index signature. A label the app does not declare
   * is not on `env.databases` at runtime either — the runtime publishes an
   * entry per database the deployment carries and nothing else — so reading
   * one is a mistake TypeScript should name rather than hand back `unknown`.
   */
  export interface EnvDatabases {}

  /**
   * Composite per-request env — same object as the `env` arg of
   * `fetch(request, env, ctx)`. Plugin namespaces appear under their
   * declared keys ("db", "kv", "storage", ...); app-scoped secrets and
   * variables appear as string keys at the top level.
   *
   * Frozen: direct assignment to properties throws in strict mode.
   *
   * The interface is named (vs. a structural literal) so user code can
   * augment `env.db` with collection-typed accessors via the generated
   * `generated/zeroship/env.db.ts` module. This package owns only the
   * base runtime module shape.
   */
  export interface Env {
    // `db` and `auth` are populated by their respective augmentations.
    // Keeping them out of the base declaration lets narrower SDK
    // augmentations be the source of truth.
    [key: string]: unknown;
    /**
     * Every database the app declares. `env.db` is the primary one and
     * `env.db === env.databases[primary]` by object identity, so there is one
     * concept and one code path; a single-database app never has to look here.
     *
     * Declared HERE rather than in a generated module because the generated
     * modules augment `EnvDatabases`, and something has to hang it off `Env`
     * exactly once.
     */
    databases: EnvDatabases;
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
