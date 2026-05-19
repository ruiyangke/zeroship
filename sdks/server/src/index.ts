// Public surface of `@zeroship/server`.
//
// This package currently ships the build-time pieces: the `defineApp`
// authoring helper and the type vocabulary for the resource tree.
// Runtime helpers (`user()`, `requireRole`, `RpcError`) are reserved
// here but still throw when called.

export { defineApp } from "./define-app.js";
export {
  type AppDefinition,
  type Resource,
  type ResourceTree,
  type AuthLevel,
  type ProcedureKind,
  type ProcedureConfig,
  type ProcedureSchema,
  type RateLimit,
  type RateLimitScope,
  type CacheControl,
  type CorsConfig,
  type RedirectAction,
  type StaticAction,
  type RpcConfig,
  type RpcDefaults,
  type Timeout,
  type DefinedApp,
  // B3 capability typing
  type QueryCtx,
  type MutationCtx,
  type ActionCtx,
  type QueryCtxDb,
  type MutationCtxDb,
  type ReadOnlyCollection,
  type ProcedureRef,
  DEFINE_APP_MARKER,
} from "./types.js";

// ── Procedure wrapper markers ─────────────────────────────────────────
//
// RPC discovery is explicit now: the vite-plugin's transform statically
// recognizes these wrapper names (any of `procedure`, `query`,
// `mutation`, `stream`, `subscription`) when imported from
// `@zeroship/server`; only exports whose initializer is one such call
// become RPCs.
//
// Plain `export function helper(...)` and `export const x = ...` stay
// private to the server bundle.
//
// Authoring shape:
//
//   import { procedure, query, mutation, z } from "@zeroship/server";
//
//   "use server";
//
//   export const list = query(async () => db.todos.find({}));
//   export const greet = procedure(
//     async (name: string) => `hi ${name}`,
//     { id: "greet" },
//   );
//   export const charge = mutation(
//     async (input: ChargeArgs) => stripe.charge(input),
//     { id: "charge", input: z.object({ amount: z.number() }) },
//   );
export { procedure, query, mutation, action, stream, subscription } from "./wrappers.js";

// Re-exports of the runtime substrate (the synthetic `"zeroship"`
// module). One import surface for user code — @zeroship/server covers
// wrappers + bindings + composition + per-request state. The
// `"zeroship"` virtual module stays as the internal substrate that
// other SDKs (@zeroship/db, @zeroship/auth, @zeroship/migrations)
// import from, but user-facing app code shouldn't need to know it
// exists.
export {
  env,
  waitUntil,
  getRequest,
  runQuery,
  runMutation,
  currentUser,
  currentRequestId,
  currentTraceId,
  currentSignal,
  currentHeaders,
  currentIdempotencyKey,
} from "zeroship";

// `__makeServerProcedure` SSR adapter.
//
// The vite-plugin's SSR-enabled-app variant wraps each user procedure
// with `__makeServerProcedure(impl, meta)` so the same React component
// code (`list.useQuery(...)`) works on both server and client. On the
// server, hooks call the impl directly (no HTTP) and stash results in
// a per-request QueryClient; the worker dehydrates that to JSON for
// the client to hydrate.
//
// Symmetric with `__makeProcedure` from `@zeroship/rpc-client`.
export { __makeServerProcedure } from "./make-server-procedure.js";
export type {
  ServerProcedureMeta,
  ServerProcedureFn,
  ServerQueryProcedure,
  ServerMutationProcedure,
  ServerStreamProcedure,
  ServerSubscriptionProcedure,
  ServerActionProcedure,
} from "./make-server-procedure.js";

// ── Zod re-export (optional peer dep) ────────────────────────────────
//
// Procedures opt into runtime input/output validation by setting
// `fn.config.input` / `fn.config.output` to a Zod schema. Zod is an
// **optional** peer dependency — users who never declare schemas
// don't need Zod installed.
//
// We re-export `z` for convenience (`import { z } from "@zeroship/server"`)
// via a dynamic `import()` so consumers who never call `z.*` still load
// `@zeroship/server` without Zod present. When Zod is missing, accessing
// any property on `z` throws a friendly install hint.
//
// Implementation note: top-level await is part of ES2022; the package's
// `tsconfig.json` targets ES2022 and Node 18+ supports it natively.

// Use the dynamic-import pattern so TS doesn't require `zod` to be
// installed at type-check time when the consumer doesn't use schemas.
// @ts-ignore — zod is an optional peer; types may not resolve.
type ZodNamespace = typeof import("zod")["z"];

let _zodModule: { z: ZodNamespace } | null = null;
try {
  // @ts-ignore — optional peer dep; absent in projects that never use schemas.
  _zodModule = (await import("zod")) as { z: ZodNamespace };
} catch {
  // Zod not installed — that's OK as long as the user never accesses `z`.
  _zodModule = null;
}

const ZOD_MISSING_MSG =
  "[zeroship] `z` is unavailable: zod is an optional peer dependency. " +
  "Run `npm install zod` to declare schemas via `fn.config.input` / `fn.config.output`.";

/**
 * Re-export of Zod's `z` namespace. `import { z } from "@zeroship/server"`
 * gives you the same object as `import { z } from "zod"` when Zod is
 * installed. When Zod is absent, every access throws with an install
 * hint — this lets callers who never declare schemas avoid the dep.
 */
export const z: ZodNamespace = _zodModule
  ? _zodModule.z
  : (new Proxy(
      {},
      {
        get(_target, prop) {
          throw new Error(`${ZOD_MISSING_MSG} (accessed: ${String(prop)})`);
        },
      },
    ) as unknown as ZodNamespace);

// ── Runtime Helper Stubs ─────────────────────────────────────────────
// Exported so user code can import them today; calling them throws.
//
// The stub form reserves the name and shape so user code can adopt the
// API before the runtime wiring exists.

const NOT_IMPLEMENTED = "Not implemented in this build of @zeroship/server";

/** Returns the request's authenticated user once runtime auth wiring exists. */
export function user(): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Returns the request's user or `null` for anonymous callers once wired. */
export function userOrNull(): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Throws `PERMISSION_DENIED` if the user lacks the role once wired. */
export function requireRole(_role: string): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Structured RPC error with a fixed code enum. */
export class RpcError extends Error {
  constructor(
    public readonly code: string,
    message: string,
    public readonly details?: unknown,
  ) {
    super(message);
    this.name = "RpcError";
  }
}
