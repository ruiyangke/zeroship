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

/** Constructor shape a durable workflow class satisfies. */
export type WorkflowCtor = { new (): { run?: unknown }; name?: string };

/**
 * Is this export a durable workflow class?
 *
 * Structural, not `instanceof Workflow`, and deliberately so: the check runs
 * inside the synthetic server entry and inside dev's namespace walk, neither
 * of which may import `@zeroship/workflows` (apps that use no workflows must
 * not be forced to depend on it). A `Workflow` subclass is the only thing an
 * app exports that is a function whose `prototype.run` is a method — plain
 * RPC procedures are arrow functions or wrapper-call results and carry no
 * prototype method.
 *
 * A false positive costs nothing: today every callable non-`default` export is
 * rolled into the RPC dict keyed by its export name, and a class rolled in
 * that way is not in `manifest.resources`, so it is already unreachable. A
 * false negative is the failure that matters, which is why the test is on
 * shape rather than on a brand the minifier or a duplicated package copy could
 * break.
 */
export function isWorkflowClass(value: unknown): value is WorkflowCtor {
  if (typeof value !== "function") return false;
  const proto = (value as { prototype?: unknown }).prototype;
  if (proto == null || typeof proto !== "object") return false;
  return typeof (proto as { run?: unknown }).run === "function";
}

/**
 * Collect the workflow classes a user module namespace exports, keyed by
 * EXPORT name.
 *
 * The export name is authoritative because that is what `env.workflows.<Name>`
 * addresses and what the deploy manifest declares. `Class.name` is not: a
 * production build binds every class to a minified identifier
 * (`var Pi = class extends ... {}`), so `DoubleChild.name` is `"Pi"` in the
 * `.zship` and `"DoubleChild"` under `pnpm dev`. `step.call(Child, ...)` reads
 * `Class.name` to address the child (dispatcher.ts `call`), so the name is
 * pinned to the export key here -- without it, child workflows resolve to a
 * minified identifier in production and to the source name in dev, and the two
 * sides disagree on a value the creator never wrote.
 */
export function collectWorkflowClasses(mod: unknown): Record<string, WorkflowCtor> {
  const modObj = (mod ?? {}) as Record<string, unknown>;
  const out: Record<string, WorkflowCtor> = {};
  for (const name of Object.keys(modObj)) {
    if (name === "default" || name === "fetch") continue;
    const value = modObj[name];
    if (!isWorkflowClass(value)) continue;
    if (value.name !== name) {
      // `Function.prototype.name` is configurable, so this is a legal rename.
      try {
        Object.defineProperty(value, "name", { value: name, configurable: true });
      } catch {
        // A frozen class keeps its own name; the workflows dict below is still
        // correct, only `step.call` on THAT class would address the old name.
      }
    }
    out[name] = value;
  }
  const userDefault = modObj.default;
  if (userDefault && typeof userDefault === "object") {
    const declared = (userDefault as { workflows?: unknown }).workflows;
    if (declared && typeof declared === "object") {
      for (const [name, value] of Object.entries(declared as Record<string, unknown>)) {
        if (isWorkflowClass(value)) out[name] = value;
      }
    }
  }
  return out;
}

export interface NormalizedUserModule {
  fetch:
    | ((request: Request, env: unknown, ctx: unknown) => Promise<Response> | Response)
    | undefined;
  rpc: Record<string, (input: unknown, ctx: unknown) => unknown>;
  /**
   * Durable workflow classes exported by the module, keyed by export name.
   * The runtime's workflow dispatch resolves the replayed class from here.
   */
  workflows: Record<string, WorkflowCtor>;
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
  const workflows = collectWorkflowClasses(mod);

  for (const name of Object.keys(modObj)) {
    if (name === "default" || name === "fetch") continue;
    const fn = modObj[name];
    if (typeof fn !== "function") continue;
    // A workflow class is a function, so without this it would be published as
    // an RPC procedure named after the class. It is not one: it has no wrapper
    // config, never appears in `manifest.resources`, and calling it would
    // invoke a constructor without `new`.
    if (isWorkflowClass(fn)) continue;
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
    workflows,
    userDefault,
  };
}
