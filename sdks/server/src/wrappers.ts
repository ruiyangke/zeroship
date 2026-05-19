// Wrapper-marker helpers — `procedure` / `query` / `mutation` / `action`
// / `stream` / `subscription`.
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
//
// B3 capability typing. The `ctx` parameter was removed entirely —
// composition + per-request state are imports from `@zeroship/server`
// (`runQuery` / `runMutation` / `currentUser` / `fetch` from
// `globalThis`). Wrappers now take `(input)` only and the runtime
// gate (`crates/runtime/src/rpc/capability.rs`) enforces the
// query/mutation/action capabilities at request time. `procedure()`
// remains the kind-inferred generic form; the four named wrappers
// pin the kind explicitly. `stream` / `subscription` map to `action`
// capability internally.

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
// `Handler` is the upper-bound constraint shared by every wrapper —
// the wrappers are identity functions, so any user handler shape
// must be assignable to it. The variadic-`any` parameter list keeps
// arity contravariance permissive: a concrete `(input: T) => U`,
// `() => U`, or async-generator factory all satisfy it without the
// user seeing a confusing `(...args: never[])` in hover. Return is
// `unknown` (broader than `any`) so the actual return-type narrowing
// happens through the wrapper's own generic parameters.
type Handler = (...args: any[]) => unknown;

/** Wrapper-marker name. `procedure` is the generic form (no implicit kind);
 *  the others tag the procedure's kind. */
type WrapperMarker =
  | "procedure"
  | "query"
  | "mutation"
  | "action"
  | "stream"
  | "subscription";

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
 * `query`/`mutation`/`action`/`stream` (or when you want the transform
 * to fall back to its name-based heuristic: `get*`/`list*`/etc →
 * query, default → mutation, async generator → stream).
 *
 *   export const greet = procedure(async (name: string) => `hi ${name}`);
 *   export const greet2 = procedure(handler, { id: "greet-v2" });
 *
 * Backwards compatibility: `procedure()` keeps its broad `Handler`
 * type — its `ctx` parameter (the second arg, if any) is whatever the
 * user declares. Capability-wise this maps to `action` semantics
 * (most permissive). Existing user code using `procedure(...)` keeps
 * compiling without changes.
 */
export function procedure<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "procedure", config);
}

/**
 * Read-only RPC procedure. Implies `kind: "query"`.
 *
 * Capability: read DB, call `runQuery`. NOT allowed: DB writes,
 * `fetch`, `runMutation`. Enforced by the runtime capability gate
 * (`crates/runtime/src/rpc/capability.rs`); a query attempting a
 * forbidden op fails request-time with `code: "capability_violation"`.
 * The handler runs inside a `BEGIN ISOLATION LEVEL READ COMMITTED
 * READ ONLY` Postgres tx auto-opened by the SSR dispatcher.
 */
export function query<TIn, TOut>(
  handler: (input: TIn) => Promise<TOut> | TOut,
  config?: ProcedureConfig,
): typeof handler {
  return attach(handler as unknown as Handler, "query", config) as typeof handler;
}

/**
 * Side-effecting RPC procedure. Implies `kind: "mutation"`.
 *
 * Capability: read+write DB, call `runQuery`. NOT allowed: `fetch`,
 * `runMutation` (mutations are already atomic; nesting is a footgun).
 * Use `action` if you need external HTTP. The handler runs inside a
 * `BEGIN ISOLATION LEVEL READ COMMITTED READ WRITE` Postgres tx
 * (override via `config.isolation`).
 */
export function mutation<TIn, TOut>(
  handler: (input: TIn) => Promise<TOut> | TOut,
  config?: ProcedureConfig,
): typeof handler {
  return attach(handler as unknown as Handler, "mutation", config) as typeof handler;
}

/**
 * Most-permissive RPC procedure. Implies `kind: "action"`.
 *
 * Capability: `fetch` (outbound HTTP), `runQuery`, `runMutation`. No
 * auto-tx — actions can hold open external I/O for minutes; wrapping
 * them in a Postgres tx would block other writers. Compose database
 * work via `runQuery` / `runMutation` so each step is its own tx.
 *
 *   import { action, runQuery, runMutation } from "@zeroship/server";
 *
 *   export const sendEmail = action(async (args) => {
 *     const user = await runQuery(getUser, { id: args.userId });
 *     await fetch("https://email-svc/send", { ... });
 *     await runMutation(recordEmailSent, { userId: args.userId });
 *   });
 */
export function action<TIn, TOut>(
  handler: (input: TIn) => Promise<TOut> | TOut,
  config?: ProcedureConfig,
): typeof handler {
  return attach(handler as unknown as Handler, "action", config) as typeof handler;
}

/**
 * Streaming RPC procedure. Implies `kind: "stream"`. The handler must
 * be an async generator (or any async iterator factory).
 *
 * Capability-wise maps to `action` (no surrounding DB tx, `fetch`
 * allowed) — see §B3 "Streaming wrappers and reactivity". The
 * handler signature is left broad so existing async-generator code
 * keeps compiling.
 */
export function stream<H extends Handler>(handler: H, config?: ProcedureConfig): H {
  return attach(handler, "stream", config);
}

/**
 * Long-lived subscription. Same wire shape as `stream` — the handler
 * yields events forever. Reserved for the WebSocket / Server-Sent
 * Events fan-out path; behaves identically to `stream` until the
 * subscription wire ships. Capability maps to `action`.
 */
export function subscription<H extends Handler>(
  handler: H,
  config?: ProcedureConfig,
): H {
  return attach(handler, "subscription", config);
}
