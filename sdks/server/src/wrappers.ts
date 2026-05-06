// Wrapper-marker helpers — `procedure` / `query` / `mutation` / `stream` /
// `subscription`.
//
// Discovery uses a two-step opt-in now:
//
//   1. The file declares a top-level `"use server"` directive (file-level
//      ECMAScript Directive Prologue). The vite-plugin's transform skips
//      anything else.
//   2. Inside such a server module, only exports whose initializer is a
//      call to one of these wrappers are registered as RPC procedures.
//      Plain `export function helper(...)` and `export const x = ...`
//      stay private.
//
// At build time the vite-plugin reads the wrapper identity statically
// from the AST (it walks ImportDeclarations and resolves the local
// binding to a wrapper name) — see `sdks/vite-plugin/src/transform.ts`.
//
// At runtime each wrapper is an identity function. It returns the
// handler unchanged, optionally attaching `.config` and a `__zsKind`
// fallback tag. The tag is for any code path that needs to dispatch on
// kind without re-reading the AST (the transform reads kind statically
// via the wrapper name; this is belt-and-suspenders).

// Re-use the canonical config type from `./types.ts`. The wrappers and
// the legacy `<fnName>.config = { ... }` assignment surface produce
// indistinguishable runtime objects, so the manifest emitter +
// synthetic SSR entry's config-extraction code keeps working unchanged.
import type { ProcedureConfig } from "./types.js";
export type { ProcedureConfig } from "./types.js";

/**
 * The marker the vite-plugin transform recognizes as a "this export
 * is an RPC procedure" wrapper. Each variant tags `__zsKind` so
 * runtime consumers can dispatch on kind without re-reading the AST.
 *
 * Type signature: identity. The wrapped value IS the handler — calling
 * `wrappedFn(input, ctx)` is the same as calling the original. We avoid
 * proxying or method binding so handlers that close over `this` (rare,
 * but legal) keep working.
 */
type Handler = (...args: never[]) => unknown;

/** Wrapper-marker name. `procedure` is the generic form (no implicit kind);
 *  the four others tag the procedure's kind. */
type WrapperMarker = "procedure" | "query" | "mutation" | "stream" | "subscription";

/**
 * Identity wrapper. Returns the handler unchanged, with optional
 * `.config` and a non-enumerable `__zsKind` tag attached (fallback for
 * runtime kind dispatch — the build-time transform reads kind
 * statically from the wrapper name).
 *
 * `procedure()` (the generic form) does NOT write a kind onto
 * `config.kind`; it leaves the kind to the transform's name-based
 * inference (`get*`/`list*`/etc → query, default → mutation, async
 * generator → stream). The other four wrappers DO write the kind so
 * `<fnName>.config.kind` reflects the wrapper choice.
 */
function attach<H extends Handler>(
  handler: H,
  marker: WrapperMarker,
  config?: ProcedureConfig,
): H {
  try {
    const baseConfig: ProcedureConfig | undefined = config;
    // For typed wrappers (query/mutation/stream/subscription) the kind
    // is implied. The user's explicit `config.kind` wins over the
    // wrapper default. For the generic `procedure` marker we leave kind
    // undefined so the transform's name-based heuristic kicks in.
    if (marker !== "procedure") {
      const merged: ProcedureConfig =
        baseConfig === undefined
          ? { kind: marker }
          : baseConfig.kind === undefined
            ? { ...baseConfig, kind: marker }
            : baseConfig;
      Object.defineProperty(handler, "config", {
        value: merged,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    } else if (baseConfig !== undefined) {
      Object.defineProperty(handler, "config", {
        value: baseConfig,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
    // Always tag — runtime fallback for code paths that need kind
    // without re-reading the AST (e.g. dynamic registries).
    Object.defineProperty(handler, "__zsKind", {
      value: marker,
      enumerable: false,
      configurable: true,
      writable: true,
    });
  } catch {
    /* frozen function — silently no-op */
  }
  return handler;
}

/**
 * Generic procedure marker. Use when the kind isn't naturally
 * `query`/`mutation`/`stream` (or when you want the transform to fall
 * back to its name-based heuristic: `get*`/`list*`/etc → query, default
 * → mutation, async generator → stream).
 *
 *   export const greet = procedure(async (name: string) => `hi ${name}`);
 *   export const greet2 = procedure(handler, { id: "greet-v2" });
 */
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "procedure", config);
}

/** Read-only RPC procedure. Implies `kind: "query"`. */
export function query<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "query", config);
}

/** Side-effecting RPC procedure. Implies `kind: "mutation"`. */
export function mutation<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "mutation", config);
}

/**
 * Streaming RPC procedure. Implies `kind: "stream"`. The handler must
 * be an async generator (or any async iterator factory).
 */
export function stream<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "stream", config);
}

/**
 * Long-lived subscription. Same wire shape as `stream` — the handler
 * yields events forever. Reserved for the WebSocket / Server-Sent
 * Events fan-out path; behaves identically to `stream` until the
 * subscription wire ships.
 */
export function subscription<H extends Handler>(
  handler: H,
  config?: ProcedureConfig,
): H {
  return attach(handler, "subscription", config);
}
