// Public types for `@zeroship/server`. The current surface is the
// `defineApp({ resources })` authoring tree plus the type vocabulary
// for procedure metadata. Most fields here are honored by the build's
// manifest emitter; some runtime and gateway enforcement is still
// reserved for future wiring.
//
// Validation: procedures opt into runtime input/output validation by
// setting `fn.config.input` / `fn.config.output` to a Zod schema (or
// any object with a `.parse(input)` method). Zod is a peer dependency
// — only required when you actually declare a schema. The synthetic
// SSR entry's dispatch reads these values at request time and calls
// `.parse()` on them; failures throw a structured INVALID_ARGUMENT
// envelope (status 400) carrying ZodError.issues to the wire.

/**
 * Structural shape of any object accepted as a procedure schema. We
 * type schemas as "anything with `.parse()`" so the types work with
 * Zod, Valibot, or a custom validator without forcing a Zod import
 * here. Zod schemas (`z.ZodTypeAny`) match structurally.
 */
export interface ProcedureSchema<T = unknown> {
  parse(input: unknown): T;
}

/** Authentication level required to reach a resource. */
export type AuthLevel = "anon" | "user" | "admin";

/** Discriminator on RPC procedures.
 *
 * The five kinds split into two axes:
 *
 *  - **Capability** (B3 from `docs/proposals/zeroship-db.md`):
 *    - `query`     — DB reads only, no `fetch()`.
 *    - `mutation`  — DB read + write, no `fetch()`.
 *    - `action`    — full surface: `fetch()`, `ctx.runQuery`,
 *                    `ctx.runMutation`. No surrounding transaction.
 *      `procedure()` (the generic wrapper) maps to the `action`
 *      capability for backwards compatibility — existing user code
 *      keeps working.
 *  - **Wire shape** (orthogonal to capability):
 *    - `stream` / `subscription` — long-lived async-iterator returns.
 *      Internally treated as `action` capability (no DB tx; `fetch`
 *      allowed) but reported here so the manifest emitter and SSR
 *      adapter can dispatch on wire shape.
 */
export type ProcedureKind =
  | "query"
  | "mutation"
  | "action"
  | "stream"
  | "subscription";

/**
 * Shape of `<fn>.config = { ... }` declarations in user code.
 *
 * The `id` field pins the wireId (otherwise the bare export name is
 * the default; production builds reject implicit ids). `input` /
 * `output` accept Zod schemas or any object with `.parse()` —
 * declaring them opts the procedure into runtime validation.
 *
 * Other fields mirror the manifest's RPC resource shape so the build
 * can roll them up without translation.
 */
export interface ProcedureConfig<TIn = unknown, TOut = unknown> {
  /** Pin the wireId; required in production. */
  id?: string;
  /** Override the auto-inferred kind (`query`/`mutation`/...). */
  kind?: ProcedureKind;
  /** Mutations can opt into idempotency-key dedupe. */
  idempotent?: boolean;
  /**
   * Idempotency-key TTL override. Default 24 h; max 7 d
   * (`{ hours: 168 }`). Only meaningful with `idempotent: true`.
   */
  idempotencyTtl?: { hours?: number };
  /** Authentication required to reach this procedure. */
  auth?: AuthLevel;
  /** Per-procedure rate-limit override. */
  rateLimit?: RateLimit;
  /** Per-procedure max body bytes. */
  maxInputBytes?: number;
  /**
   * Per-procedure handler timeout. The gateway sets a request-level
   * deadline; the worker's `ctx.signal` aborts when it expires. On
   * abort: HTTP 504 `code: "TIMEOUT"`. Inheritance merge rule: min
   * wins (matches `rate_limit`, `max_input_bytes`).
   */
  timeout?: Timeout;
  /**
   * Pre-handler middleware names. These are carried through the manifest
   * now and will resolve against the app's middleware registry once the
   * runtime middleware chain is wired.
   */
  middleware?: string[];
  /**
   * Zod schema (or any `.parse()`-shaped object) validating the first
   * argument. When set, the synthetic SSR entry calls
   * `input.parse(args[0])` before invoking the handler; failures
   * throw an INVALID_ARGUMENT error to the wire.
   */
  input?: ProcedureSchema<TIn>;
  /**
   * Zod schema for the handler's return value. Validates in dev only
   * (NODE_ENV !== "production") for a cheap correctness check; the
   * production hot path skips it.
   */
  output?: ProcedureSchema<TOut>;
}

/** Where rate-limit buckets are keyed. */
export type RateLimitScope = "ip" | "user" | "session" | "app";

export interface RateLimit {
  rpm?: number;
  rps?: number;
  per?: RateLimitScope;
}

export interface CacheControl {
  maxAge?: number;
  swr?: number;
  immutable?: boolean;
  staleOnError?: boolean;
}

export interface CorsConfig {
  allowOrigins?: string[];
  allowMethods?: string[];
  allowHeaders?: string[];
  exposeHeaders?: string[];
  allowCredentials?: boolean;
  maxAge?: number;
}

export interface RedirectAction {
  to: string;
  status?: number;
}

export interface StaticAction {
  try: string[];
  cache?: CacheControl;
  status?: number;
}

/**
 * One node in the authoring resource tree. Mirrors the manifest shape
 * described in `docs/proposals/rpc.md` §7. Children are an
 * authoring convenience; the build flattens them to fully-qualified
 * keys.
 *
 * Mutually exclusive routing actions: `redirect`, `rewrite`, `static`.
 */
export interface Resource {
  // ── Routing action (at most one) ──
  redirect?: string | RedirectAction;
  rewrite?: string;
  static?: StaticAction;

  // ── Policy ──
  auth?: AuthLevel;
  cors?: CorsConfig;
  cache?: CacheControl;
  rateLimit?: RateLimit;
  csrfOrigins?: string[];
  idempotent?: boolean;
  middleware?: string[];
  maxInputBytes?: number;
  timeout?: Timeout;
  publiclyAccessible?: boolean;

  // ── Override marker — required when shadowing inherited fields ──
  override?: string[];

  // ── RPC procedure metadata (RPC keys only) ──
  kind?: ProcedureKind;
  cacheable?: boolean;

  // ── Authoring sugar — flattened by the build ──
  children?: ResourceTree;
}

/** Map from key (URL path, `rpc:<id>`, or `*` root) to a resource. */
export type ResourceTree = Record<string, Resource>;

/** Per-procedure timeout. */
export interface Timeout {
  ms?: number;
}

/**
 * App-level RPC defaults. Procedures inherit these unless overridden by
 * `fn.config` / module-level `$config`. `docs/proposals/rpc.md` §1
 * defines the resolution order:
 * `fn.config` → module `$config` → `defineApp({ rpc: { defaults } })` →
 * built-in defaults.
 */
export interface RpcDefaults {
  auth?: AuthLevel;
  rateLimit?: RateLimit;
  timeout?: Timeout;
  maxInputBytes?: number;
}

/** App-level RPC config. */
export interface RpcConfig {
  /** Defaults inherited by every procedure unless overridden. */
  defaults?: RpcDefaults;
  /**
   * If `true`, error redaction is disabled in production builds —
   * thrown errors keep their full message on the wire. Useful for
   * staging environments where you want production-like routing but
   * readable errors. See `docs/proposals/rpc.md` §16 for the
   * production redaction rule.
   */
  dev?: boolean;
}

export interface NetRequest {
  host: string;
  port: number;
  reason: string;
}

export interface NetConfig {
  requests?: NetRequest[];
}

/** Root argument for `defineApp({ ... })`. */
export interface AppDefinition {
  resources?: ResourceTree;
  /**
   * Inert outbound TCP request hints. These do not grant access; the control
   * plane diffs them against operator-authored grants for review.
   */
  net?: NetConfig;
  /**
   * App-level RPC config. Per-procedure overrides are pulled from
   * each procedure's `fn.config`; this block sets the app-wide
   * defaults inherited by procedures that don't override.
   */
  rpc?: RpcConfig;
}

/** Internal marker on the object `defineApp` returns. */
export const DEFINE_APP_MARKER: unique symbol = Symbol.for("zeroship/defineApp");

/** Shape of the object `defineApp` returns. */
export interface DefinedApp {
  readonly [DEFINE_APP_MARKER]: true;
  readonly definition: AppDefinition;
}
