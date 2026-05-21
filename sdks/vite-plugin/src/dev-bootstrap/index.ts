/**
 * Dev bootstrap — entry module for the zeroship V8 runtime in dev mode.
 * Bundled into `dist/dev-bootstrap.js` by esbuild.
 *
 * Stage 5b — the dev-bootstrap is a NORMALISER. Dispatch (input
 * validation, capability frame, auto-tx, stream framing, output
 * validation) lives in the runtime's `__zsDispatch` (Stage 5a,
 * `crates/runtime/src/bootstrap/rpc_dispatch.js`).
 *
 * Wire shape — `default`:
 *   schema   Surfaced via `maybeRegisterSchema` once per ModuleRunner
 *            lifetime (HMR / dep-reoptimize resets the runner, which
 *            resets the schema-registered flag).
 *   fetch    Async thunk that re-imports the user module through the
 *            ModuleRunner per request and dispatches to either the
 *            kernel-managed /_zs/v1/<id> path (via __zsDispatch) or the
 *            user's own `default.fetch`.
 *   rpc      Function-shape (`(name, input, ctx) => ...`). Dev needs to
 *            re-resolve per request because HMR may have replaced the
 *            module namespace. The runtime's Stage-5a back-compat path
 *            accepts function-shape `default.rpc` through 5d.
 *
 * Procedure discovery: the transform plugin appends
 * `globalThis.__register(wireId, fn)` to every server module's emitted
 * source. Importing the entry triggers those side-effects; the registry
 * is the authoritative dispatch table in dev. Last-write-wins so HMR
 * replacements land cleanly. The normaliser ALSO walks the user
 * module's namespace exports as a fallback when the transform didn't
 * fire (e.g. raw JS deploys that bypass the marker).
 */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";
// Typed accessors for the SDK-internal globals shared with
// `@zeroship/db`. Importing them statically gives us one source of truth
// for the names.
import {
  getPlatformReady,
  INSTALL_SCHEMA_NAME,
} from "@zeroship/db/internal";

const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;
// Stage 2 — absolute path to the DB schema module, when Stage 1's
// resolver picked a split-file convention or the user supplied
// `schema:` to the Vite plugin. `undefined` means "no split-file
// schema" — fall back to `_zsUser.default?.schema`.
const SCHEMA_PATH: string | undefined =
  (globalThis as any).process?.env?.ZEROSHIP_SCHEMA_PATH || undefined;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;

// Registry of server functions. Populated by `__register(name, fn)` calls
// that the transform appends to server modules. Importing the user
// module triggers those side-effects. Last-write-wins so HMR
// replacements land cleanly.
const registry: Map<string, Function> = new Map();
(globalThis as any).__register = (name: string, fn: Function) => {
  registry.set(name, fn);
};
(globalThis as any).__lookup = (name: string): Function | undefined => registry.get(name);

async function getRunner(): Promise<ModuleRunner> {
  if (runner) return runner;
  if (runnerPromise) return runnerPromise;
  runnerPromise = createRunner().then((r) => {
    runner = r;
    console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);
    return r;
  }).catch((err) => {
    runnerPromise = null;
    throw err;
  });
  return runnerPromise;
}

/**
 * Import the user module, retrying once if Vite's dep optimizer
 * regenerated pre-bundled files while our ModuleRunner had a stale
 * version cached. Symptom: `The file does not exist at
 * "…/.vite/deps_zeroship/foo-HASH.js"`.
 */
async function getUserModule(): Promise<any> {
  const r = await getRunner();
  let mod: any;
  try {
    mod = await r.import(ENTRY);
  } catch (err: any) {
    const msg = String(err?.message ?? err ?? "");
    if (msg.includes("is in the optimize deps directory")) {
      console.log("[zeroship:dev] deps re-optimized, resetting runner");
      runner = null;
      runnerPromise = null;
      // Schema must be re-registered against the fresh runner because
      // the new runtime instance carries a separate `@zeroship/db` copy
      // (see maybeRegisterSchema for the `instanceof` rationale).
      schemaRegistered = false;
      const fresh = await getRunner();
      mod = await fresh.import(ENTRY);
    } else {
      throw err;
    }
  }
  await maybeRegisterSchema(mod);
  return mod;
}

// Set once per ModuleRunner lifetime — schema auto-discovery is
// idempotent on the SDK side, but re-running registerModel for every
// RPC dispatch is wasted work. HMR / dep-reoptimize cycles reset the
// flag in lockstep with the runner reset above.
let schemaRegistered = false;

/**
 * Schema auto-registration in dev mode.
 *
 * Resolution order:
 *   1. `ZEROSHIP_SCHEMA_PATH` env var set → import that module through
 *      the ModuleRunner, use its `default.schema ?? default`.
 *   2. Fall back to `mod.default.schema` (the standard convention).
 *   3. Nothing found → no-op.
 *
 * Both resolutions hand the schema to `@zeroship/db::_installSchema`
 * with `installOnEnvDb: true` so `env.db.<collection>` resolves to a
 * typed Collection wrapper before the first RPC dispatch runs.
 */
async function maybeRegisterSchema(mod: any): Promise<void> {
  if (schemaRegistered) return;
  // Load `@zeroship/db` THROUGH the ModuleRunner so the `TypeBuilder`
  // classes match the ones the user's module instantiated. Importing
  // the SDK from the bundled dev-bootstrap would give us a DIFFERENT
  // class (esbuild's bundled copy vs Vite's loaded copy), breaking
  // `instanceof` checks inside `_installSchema`.
  let r: ModuleRunner;
  try {
    r = await getRunner();
  } catch {
    schemaRegistered = true;
    return;
  }

  // Step 1 — split-file path.
  let schema: unknown;
  let provenance: "split-file" | "default-export" | "none" = "none";
  if (SCHEMA_PATH) {
    try {
      const schemaMod: any = await r.import(SCHEMA_PATH);
      const candidate =
        (schemaMod && schemaMod.default && typeof schemaMod.default === "object"
          ? (schemaMod.default.schema ?? schemaMod.default)
          : undefined);
      if (candidate && typeof candidate === "object") {
        schema = candidate;
        provenance = "split-file";
      }
    } catch (e: any) {
      console.error(
        `[zeroship:dev] split-file schema import failed (${SCHEMA_PATH}):`,
        e?.message ?? e,
      );
    }
  }

  // Step 2 — entry-default convention fallback.
  if (!schema) {
    const defaultExport = mod && mod.default;
    if (
      defaultExport &&
      typeof defaultExport === "object" &&
      defaultExport.schema &&
      typeof defaultExport.schema === "object"
    ) {
      schema = defaultExport.schema;
      provenance = "default-export";
    }
  }

  if (!schema) {
    schemaRegistered = true;
    return;
  }

  try {
    const dbSdk: any = await r.import("@zeroship/db");
    const installFn = dbSdk[INSTALL_SCHEMA_NAME];
    if (typeof installFn === "function") {
      installFn(schema, { installOnEnvDb: true });
      console.log(`[zeroship:dev] registered schema from ${provenance}`);
    }
  } catch (e: any) {
    console.error(`[zeroship:dev] ${provenance} schema registration failed:`, e?.message ?? e);
  } finally {
    schemaRegistered = true;
  }
}

// Kick off connection immediately.
getRunner()
  .then((r) => startHmrPoll(r))
  .catch((e) => console.error("[zeroship:dev] Runner init failed:", e));

/**
 * Poll Vite for changed files every 500ms and invalidate the
 * ModuleRunner's evaluated-module cache for each changed path. Causes
 * the next import() to re-fetch from Vite (which re-transforms).
 */
function startHmrPoll(runner: ModuleRunner) {
  const viteWsUrl = (globalThis as any).process?.env?.ZEROSHIP_VITE_WS;
  if (!viteWsUrl) return;

  const viteOrigin = viteWsUrl
    .replace(/^ws:/, "http:")
    .replace(/^wss:/, "https:")
    .replace(/\/__zeroship_hmr$/, "");

  const pollUrl = `${viteOrigin}/__zeroship_hmr_check`;

  setInterval(async () => {
    // The poll is dev-kernel infrastructure — bypass the active
    // capability frame so the fetch isn't refused if it fires inside a
    // query/mutation handler's await window.
    const ck = (globalThis as any).__zsClearKind;
    const xk = (globalThis as any).__zsExitKind;
    const tok = (typeof ck === "function") ? ck() : -1;
    try {
      const resp = await fetch(pollUrl);
      const { changed } = await resp.json() as { changed: string[] };

      if (changed.length === 0) return;

      for (const file of changed) {
        const mods = runner.evaluatedModules;
        for (const id of [file, `/${file}`, file.replace(/\\/g, "/")]) {
          const mod = mods.getModuleById(id);
          if (mod) {
            mods.invalidateModule(mod);
          }
        }
      }

      console.log(`[zeroship:hmr] ${changed.length} module(s) updated`);
    } catch {
      // Vite not ready or restarting — silently ignore
    } finally {
      if (tok >= 0 && typeof xk === "function") xk(tok);
    }
  }, 500);
}

// ── Standard-shape normaliser ──────────────────────────────────────────────

/**
 * Build a dict-shape `_zsRpc` from the freshly imported user module.
 *
 * Resolution order:
 *   1. `mod.default.rpc` (if a plain object) is the base — pass-through
 *      verbatim so users can declare procedures inline.
 *   2. Named exports rolled in (named exports win on key conflict).
 *   3. Registry entries (transform-appended __register calls) merged
 *      last; they're authoritative for the transformed-source path. The
 *      transform fires AFTER the module evaluates, so the registry
 *      reflects the latest HMR state.
 *
 * Returns the user's own default-object plus the merged rpc dict and
 * the resolved fetch handler.
 */
function buildStandard(mod: any): { schema: unknown; fetch: any; rpc: Record<string, Function> } {
  const userDefault = (mod && typeof mod.default === "object" && mod.default) || {};

  const rpc: Record<string, Function> =
    (typeof userDefault.rpc === "object" && userDefault.rpc != null)
      ? { ...userDefault.rpc }
      : {};

  // Named-export procedures. The transform's `__register` mirror only
  // fires when the marker matched; bare-source named exports (raw JS
  // deploys served through dev) still need to land in the dict.
  if (mod && typeof mod === "object") {
    for (const name of Object.keys(mod)) {
      if (name === "default" || name === "fetch") continue;
      const fn = (mod as any)[name];
      if (typeof fn !== "function") continue;
      const id = (typeof (fn as any).config?.id === "string" && (fn as any).config.id) || name;
      rpc[id] = fn;
    }
  }

  // Registry entries — last-write-wins for HMR. The transform appends
  // `globalThis.__register(<wireId>, <fn>)` to every server module, so
  // this captures procedures discovered through marker-based transform
  // even when they aren't surfaced as named exports of the entry.
  for (const [name, fn] of registry) {
    rpc[name] = fn;
  }

  const fetch =
    (typeof userDefault.fetch === "function") ? userDefault.fetch :
    (mod && typeof (mod as any).fetch === "function" ? (mod as any).fetch : undefined);

  return { schema: userDefault.schema, fetch, rpc };
}

// ── default.rpc (function-shape — back-compat through 5d) ─────────────────

/**
 * Dispatcher entry point used by the runtime's kernel /_zs/v1/<id>
 * fast path. Each call re-imports the user module through the
 * ModuleRunner (HMR may have invalidated cached evaluations) and
 * delegates to `__zsDispatch(dict, name, input, ctx)`.
 *
 * Function-shape: dev's namespace may change per request, so the dict
 * is freshly resolved on every call. Production uses dict-shape because
 * the bundle is frozen.
 *
 * Note: the runtime's Stage-5a back-compat path accepts function-shape
 * `default.rpc` through Stage 5d. After 5d this falls under the
 * advanced-user override clause documented in the design proposal.
 */
async function dispatchRpc(name: string, input: unknown, ctx: unknown): Promise<unknown> {
  // Re-import per call so HMR invalidations land naturally. On the
  // FIRST call this also triggers schema registration via
  // `maybeRegisterSchema(mod)` inside `getUserModule` — that's where
  // `__zeroshipPlatformReady` first gets published.
  const mod = await getUserModule();
  const { rpc } = buildStandard(mod);

  // Await platform-readiness AFTER the schema install has had a chance
  // to publish `globalThis.__zeroshipPlatformReady`. Reading it BEFORE
  // the import would observe `undefined` on the first call (schema
  // hasn't registered yet) and skip the wait, racing the auto-tx
  // dispatcher against an in-flight DDL chain. Under pglite-socket's
  // per-connection-in-tx serialisation, opening BEGIN while
  // registerModel still holds `pg_advisory_lock` deadlocks — so we
  // gate here. No-op on the warm path (the chain has settled).
  const ready = getPlatformReady();
  if (ready && typeof ready.then === "function") {
    try { await ready; } catch { /* surfaces via the handler */ }
  }

  const dispatch = (globalThis as any).__zsDispatch;
  if (typeof dispatch !== "function") {
    // Should never happen — the runtime bootstrap installs __zsDispatch
    // before evaluating dev-bootstrap.
    throw Object.assign(new Error("__zsDispatch is not installed"), {
      status: 500,
      code: "INTERNAL",
    });
  }
  return dispatch(rpc, name, input, ctx);
}

// ── default.fetch ──────────────────────────────────────────────────────────

/**
 * WinterCG handler. Owns the /_zs/v1/<id> fall-through (for cases where
 * the kernel's RPC fast path returned an AsyncIterator — re-dispatches
 * through SSE framing). Non-/_zs/v1/ paths forward to the user's own
 * `default.fetch` (when present), or 404.
 */
async function dispatchFetch(request: Request, env: unknown, ctx: unknown): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname.startsWith("/_zs/v1/")) {
    const id = url.pathname.slice("/_zs/v1/".length);
    if (!id) return errResponse(400, "INVALID_ARGUMENT", "missing wireId");

    let input: unknown = undefined;
    if (request.method === "GET") {
      const param = url.searchParams.get("input");
      if (param) {
        try {
          const b64 = param.replace(/-/g, "+").replace(/_/g, "/");
          const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
          const e = JSON.parse(atob(padded));
          input = (e && typeof e === "object" && "json" in e) ? e.json : e;
        } catch (e: any) {
          return errResponse(400, "INVALID_ARGUMENT", `invalid base64url input: ${e?.message ?? e}`);
        }
      }
    } else if (request.method === "POST") {
      const text = await request.text();
      if (text) {
        try {
          const e = JSON.parse(text);
          input = (e && typeof e === "object" && "json" in e) ? e.json : e;
        } catch (e: any) {
          return errResponse(400, "INVALID_ARGUMENT", `invalid JSON body: ${e?.message ?? e}`);
        }
      }
    } else {
      return errResponse(405, "FAILED_PRECONDITION", `method ${request.method} not allowed on /_zs/v1/`);
    }

    return rpcAndRespond(id, input, ctx);
  }

  // Fall through to user's own default.fetch (when present).
  let mod: any;
  try {
    mod = await getUserModule();
  } catch (e: any) {
    return errResponse(500, "INTERNAL", `Module import failed: ${e?.message ?? e}`);
  }
  const { fetch: userFetch, schema: _schema } = buildStandard(mod);
  const userDefault = (mod && mod.default && typeof mod.default === "object") ? mod.default : null;
  if (typeof userFetch === "function") return userFetch.call(userDefault, request, env, ctx);
  return new Response("Not Found", { status: 404 });
}

async function rpcAndRespond(name: string, input: unknown, ctx: unknown): Promise<Response> {
  try {
    const result = await dispatchRpc(name, input, ctx);

    if (isAsyncIterator(result)) {
      // AI-SDK Data Stream Protocol — line-prefixed framing.
      const outputIsString = !!(result as any).__zsOutputIsString;
      const encoder = new TextEncoder();
      const body = new ReadableStream({
        async start(controller) {
          try {
            while (true) {
              const step = await (result as AsyncIterator<unknown>).next();
              if (step.done) {
                controller.enqueue(encoder.encode("d:{}\n"));
                break;
              }
              const v = step.value;
              if (outputIsString || typeof v === "string") {
                controller.enqueue(encoder.encode("0:" + JSON.stringify(String(v)) + "\n"));
              } else {
                controller.enqueue(encoder.encode("2:[" + JSON.stringify(v) + "]\n"));
              }
            }
          } catch (e: any) {
            const env: Record<string, unknown> = {
              message: e?.message ?? String(e),
              name: e?.name ?? "Error",
            };
            if (typeof e?.code === "string") env.code = e.code;
            if (e?.details !== undefined) env.details = e.details;
            if (typeof e?.retryable === "boolean") env.retryable = e.retryable;
            controller.enqueue(encoder.encode("e:" + JSON.stringify(env) + "\n"));
            controller.enqueue(encoder.encode("d:{}\n"));
          } finally {
            controller.close();
          }
        },
      });
      return new Response(body, {
        status: 200,
        headers: {
          "content-type": "text/event-stream",
          "cache-control": "no-cache, no-transform",
          "x-accel-buffering": "no",
        },
      });
    }

    if (result instanceof Response) return result;

    return new Response(
      JSON.stringify({ json: result === undefined ? null : result }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  } catch (err: any) {
    const status = statusFromError(err);
    const body: Record<string, unknown> = {
      message: err?.message ?? String(err),
      name: err?.name ?? "Error",
    };
    if (typeof err?.code === "string") body.code = err.code;
    if (err?.details !== undefined) body.details = err.details;
    if (typeof err?.retryable === "boolean") body.retryable = err.retryable;
    return new Response(
      JSON.stringify(body),
      { status, headers: { "content-type": "application/json" } },
    );
  }
}

// ── Helpers ────────────────────────────────────────────────────────────────

function isAsyncIterator(x: any): boolean {
  return (
    x != null &&
    typeof x === "object" &&
    typeof x[Symbol.asyncIterator] === "function" &&
    typeof x.next === "function"
  );
}

function statusFromError(e: any): number {
  const s = e?.status;
  if (typeof s === "number" && s >= 400 && s < 600) return s;
  return 500;
}

function errResponse(status: number, code: string, message: string, details?: unknown) {
  const body: Record<string, unknown> = { message, name: "Error", code };
  if (details !== undefined) body.details = details;
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

// ── Module entry ──────────────────────────────────────────────────────────

// Function-shape `default.rpc` — back-compat path the runtime keeps
// through Stage 5d. Dev needs per-call resolution because HMR may have
// replaced the user module's namespace between requests; production
// emits dict-shape because the bundle is frozen.
export default { fetch: dispatchFetch, rpc: dispatchRpc };
