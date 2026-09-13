// Dev dispatch glue retained until the native dev entry loader cuts over.
import "./dispatcher.js";
import { createFetchHandler } from "./fetch-handler.js";
import { createDevAuthProvider } from "./dev-auth.js";
import { normalizeUserModule, type NormalizedUserModule } from "./normalize.js";

declare const globalThis: {
  __zsDispatch?: (
    rpc: Record<string, unknown>,
    name: string,
    input: unknown,
    ctx: unknown,
  ) => Promise<unknown>;
  [key: string]: unknown;
};

export interface DevEntryOptions {
  loadUserModule: () => Promise<unknown>;
  registry?: Map<string, (input: unknown, ctx: unknown) => unknown>;
  getDevAuthEnv?: (name: string) => string | undefined;
}

export interface DevEntry {
  fetch: (request: Request, env: unknown, ctx: unknown) => Promise<Response>;
  rpc: (name: string, input: unknown, ctx: unknown) => unknown;
  loadWorkflow: (name: string) => Promise<unknown>;
}

export function devEntry(options: DevEntryOptions): DevEntry {
  async function loadNormalized(): Promise<NormalizedUserModule> {
    return normalizeUserModule(await options.loadUserModule(), options.registry);
  }

  async function dispatchRpcAsync(name: string, input: unknown, ctx: unknown): Promise<unknown> {
    const normalized = await loadNormalized();
    const dispatch = globalThis.__zsDispatch;
    if (typeof dispatch !== "function") {
      // Should never happen — `import "./dispatcher.js"` above
      // installs `__zsDispatch` at module-init.
      throw Object.assign(new Error("__zsDispatch is not installed"), {
        status: 500,
        code: "INTERNAL",
      });
    }
    return dispatch(normalized.rpc, name, input, ctx);
  }

  // Non-async RPC dispatcher — must NOT be declared `async`. For stream /
  // subscription procedures the kernel expects to receive the AsyncIterator
  // synchronously so its FallThrough path fires and routes the request to
  // `default.fetch` (which owns the SSE encoding via `createFetchHandler`).
  // An `async function` always wraps returns in a Promise; the kernel's
  // promise-settle path would then see `Promise<AsyncIterator>` and surface
  // "AsyncIterator from a Promise — unsupported" instead.
  //
  // Fast-sync path: look up the handler in the transform-populated registry.
  // `__register(wireId, fn)` is appended to every server module by the Vite
  // transform, so the registry is populated after the first module evaluation.
  // Any call that arrives before the first evaluation (registry empty) falls
  // back to the async path, which also fails for streams on first call — the
  // smoke always makes several non-stream RPC calls first, so the module is
  // already evaluated by the time subscribeTodos is called in practice.
  function dispatchRpc(name: string, input: unknown, ctx: unknown): unknown {
    const regFn = options.registry?.get(name) as ((input: unknown, ctx: unknown) => unknown) | undefined;
    if (regFn) {
      const regFnMeta = regFn as { kind?: string; config?: { kind?: string } };
      // Legacy `fn.config = { id: "..." }` assignments replace the
      // wrapper-attached config object, which can drop `config.kind`
      // for stream/subscription procedures. The SSR-hook patch also
      // plants a stable top-level `.kind`; honor it first so the dev
      // stream fast-path survives config replacement.
      const kind = regFnMeta.kind ?? regFnMeta.config?.kind;
      if (kind === "stream" || kind === "subscription") {
        // Return the AsyncIterator synchronously — the kernel's FallThrough
        // path routes to default.fetch (createFetchHandler) which handles
        // SSE encoding. The handler is re-invoked there via __zsDispatch;
        // calling it here (discarded) is benign for `async function*`.
        return regFn(input, ctx);
      }
    }
    // Non-streaming procedures: use the async path (returns a Promise).
    return dispatchRpcAsync(name, input, ctx);
  }

  const userFetchHandler = createFetchHandler(loadNormalized);

  // Dev-tier auth provider — owns the same-origin `/__zeroship/auth/*` endpoints in
  // self-contained dev (no gateway or external auth service). Reads its config from the spawn env
  // (`ZEROSHIP_DEV_AUTH` + `ZEROSHIP_DEV_AUTH_SECRET`, set by the Vite plugin);
  // `null` when no secret is present (e.g. a hand-run `zeroship serve`), in
  // which case `/__zeroship/auth/*` falls through to the user module unchanged.
  const devAuthEnv =
    options.getDevAuthEnv ??
    ((name: string) =>
      (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env?.[
        name
      ]);
  const devAuth = createDevAuthProvider(devAuthEnv);

  // Intercept `/__zeroship/auth/*` BEFORE the user module's fetch + the RPC
  // fall-through. The runtime's dev serve path (`dev_auth.rs`) has ALREADY
  // resolved the dev identity from the `__zeroship_dev_session` cookie and threaded
  // it through `call_fetch_handler_with_user` for THIS request — so the
  // app-facing `env.auth.getUser()` / `currentUser()` are populated server-side
  // independently of these endpoints. These endpoints exist purely to drive the
  // browser `@zeroship/auth` client (cookie lifecycle + identity projection).
  const fetchHandler = devAuth
    ? async (request: Request, env: unknown, ctx: unknown): Promise<Response> => {
        const pathname = new URL(request.url).pathname;
        if (devAuth.handles(pathname)) return devAuth.handle(request);
        return userFetchHandler(request, env, ctx);
      }
    : userFetchHandler;

  return {
    fetch: fetchHandler,
    rpc: dispatchRpc,
    // Async by necessity: the creator's module lives behind Vite's
    // ModuleRunner and is fetched over HTTP, so dev cannot hand the runtime a
    // static workflow dict the way a bundled `.zship` does. `loadNormalized()`
    // is the same path an RPC takes, so a workflow body sees the same
    // installed schema and the same HMR generation as a request handler.
    loadWorkflow: async (name: string) => {
      const normalized = await loadNormalized();
      return normalized.workflows[name];
    },
  };
}
