/**
 * Turn a user module namespace into the standard `{ fetch, rpc }` shape
 * the runtime expects. Used by `dev-entry.ts` (and indirectly by the Vite
 * plugin's synthetic SSR entry, which emits equivalent inline code — see
 * `sdks/vite-plugin/src/rpc-registry.ts`).
 *
 * Resolution order — `rpc` dict:
 *   1. `mod.default.rpc` (if a plain object) is the base — passed
 *      through verbatim so users can declare procedures inline.
 *   2. Named exports rolled in (named exports win on key conflict;
 *      they're the canonical source-level declaration).
 *   3. Caller-supplied registry entries (transform-appended
 *      `__register` calls in dev) merged last; they're authoritative
 *      for the transformed-source path and reflect the latest HMR
 *      state.
 *
 * The `default.fetch` resolution falls through to a top-level `fetch`
 * export when no `default` object is present — mirrors the namespace-
 * walk shape the Vite plugin's synthetic entry generates.
 */

export interface NormalizedUserModule {
  fetch:
    | ((request: Request, env: unknown, ctx: unknown) => Promise<Response> | Response)
    | undefined;
  rpc: Record<string, (input: unknown, ctx: unknown) => unknown>;
  /**
   * The user's own `default` object — exposed so the caller can use it
   * as the `this` value for `default.fetch(...)`. Mirrors the
   * pre-Stage-7 dev-bootstrap shape where `userFetch.call(userDefault,
   * request, env, ctx)` preserved any `this`-bound state the user
   * planted on the default object.
   */
  userDefault: Record<string, unknown> | null;
}

/**
 * Build a normalized `{ fetch, rpc, userDefault }` view of the
 * user's module namespace. Optionally merges a caller-supplied
 * registry of `wireId → handler` last; the dev path uses this to honour
 * the transform's `__register` side-effects on top of the namespace
 * walk.
 */
export function normalizeUserModule(
  mod: unknown,
  registry?: Map<string, (input: unknown, ctx: unknown) => unknown>,
): NormalizedUserModule {
  const modObj = (mod ?? {}) as Record<string, unknown>;
  const userDefault =
    (modObj.default && typeof modObj.default === "object")
      ? (modObj.default as Record<string, unknown>)
      : null;

  const userRpc = userDefault?.rpc;
  const rpc: Record<string, (input: unknown, ctx: unknown) => unknown> =
    (typeof userRpc === "object" && userRpc !== null)
      ? { ...(userRpc as Record<string, (input: unknown, ctx: unknown) => unknown>) }
      : {};

  // Named-export procedures. Walks the module namespace for callable
  // non-`default`/`fetch` exports. Mirrors the synthetic SSR entry's
  // namespace-walk shape (`sdks/vite-plugin/src/rpc-registry.ts`).
  for (const name of Object.keys(modObj)) {
    if (name === "default" || name === "fetch") continue;
    const fn = modObj[name];
    if (typeof fn !== "function") continue;
    const fnAny = fn as { config?: { id?: string } };
    const id = (typeof fnAny.config?.id === "string" && fnAny.config.id) || name;
    rpc[id] = fn as (input: unknown, ctx: unknown) => unknown;
  }

  // Registry entries — last-write-wins for HMR. The transform appends
  // `globalThis.__register(<wireId>, <fn>)` to every server module, so
  // this captures procedures discovered through marker-based transform
  // even when they aren't surfaced as named exports of the entry.
  if (registry) {
    for (const [name, fn] of registry) {
      rpc[name] = fn;
    }
  }

  const userFetch = userDefault?.fetch;
  const topFetch = modObj.fetch;
  const fetch =
    typeof userFetch === "function"
      ? (userFetch as NormalizedUserModule["fetch"])
      : typeof topFetch === "function"
        ? (topFetch as NormalizedUserModule["fetch"])
        : undefined;

  return {
    fetch,
    rpc,
    userDefault,
  };
}
