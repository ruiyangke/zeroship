// Public surface of `@zeroship/server`.
//
// This package owns app/resource configuration, the optional Zod
// convenience export, and runtime substrate re-exports from the
// synthetic `"zeroship"` module. RPC wrapper markers live in
// `@zeroship/rpc/server`.

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
  DEFINE_APP_MARKER,
} from "./types.js";

// Re-exports of the runtime substrate (the synthetic `"zeroship"`
// module).
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

// `__makeServerProcedure` metadata adapter.
//
// The vite-plugin's SSR build wraps each user procedure with
// `__makeServerProcedure(impl, meta)` so it can copy `id`, `kind`, and
// `wire` metadata onto the original export before synthetic-entry
// binding.
//
// Symmetric with the client-side procedure brander in `@zeroship/rpc`.
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

// Per-request user/role helpers + RpcError class are reserved for a
// future build of `@zeroship/server`. Use `currentUser()` (re-exported
// from the synthetic `zeroship` module above) to read the
// gateway-injected user context in the meantime.
