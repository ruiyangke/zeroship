// Public surface of `@zeroship/server`.
//
// Phase 1 ships only the build-time pieces: the `defineApp` authoring
// helper and the type vocabulary for the resource tree. Runtime helpers
// (`user()`, `requireRole`, `RpcError`) are stubs that throw — they ship
// in Phase 4 / Phase 5.

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

// ── Phase 4/5 stubs ──────────────────────────────────────────────────
// Exported so user code can import them today; calling them throws.
//
// Each export here gets a real implementation in the corresponding
// phase. The stub form reserves the name and shape so user code that
// imports them survives a phased rollout.

const NOT_IMPLEMENTED = "Not implemented in Phase 1 of @zeroship/server";

/** Phase 5: returns the request's authenticated user. */
export function user(): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Phase 5: returns the request's user or null if anon. */
export function userOrNull(): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Phase 5: throws PERMISSION_DENIED if the user lacks the role. */
export function requireRole(_role: string): never {
  throw new Error(NOT_IMPLEMENTED);
}

/** Phase 4: structured RPC error with a fixed code enum. */
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
