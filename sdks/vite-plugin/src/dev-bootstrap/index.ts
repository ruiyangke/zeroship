/**
 * Dev bootstrap — entry module for zeroship V8 runtime in dev mode.
 * Bundled into dist/dev-bootstrap.js by esbuild.
 *
 * Wire shape — `default`:
 *   fetch(request, env, ctx)  WinterCG handler. /_zs/v1/<id> requests
 *                             dispatch through `rpc`; everything else
 *                             falls through to the user's own
 *                             `default.fetch` (when present), or 404s.
 *   rpc(name, input, ctx)     Standalone kernel entry. The runtime calls
 *                             this directly when the URL matches
 *                             /_zs/v1/<id>. Looks up `name` in the
 *                             registry populated by the transform's
 *                             `__register(wireId, fn)` side-effects.
 *
 * Mirrors production's `sdks/vite-plugin/src/rpc-registry.ts` so
 * `pnpm vite` (dev) and `pnpm build` (prod) speak the same protocol.
 */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";
// Typed accessors for the two cross-module globals shared with
// `@zeroship/db` and the production synthetic SSR entry. Importing them
// statically (vs reading off `globalThis as any`) gives us one source
// of truth for the names. The globals themselves are shared across
// every copy of the SDK in the V8 isolate — `getPlatformReady()` here
// observes what `_installSchema` (run via the ModuleRunner) wrote.
import {
  getPlatformReady,
  getSchemaInit,
  INSTALL_SCHEMA_NAME,
} from "@zeroship/db/internal";

const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;
// Stage 2 — absolute path to the DB schema module, when Stage 1's
// resolver picked a split-file convention or the user supplied
// `schema:` to the Vite plugin. `undefined` means "no split-file
// schema" — dev-bootstrap then falls back to `_zsUser.default?.schema`
// (the legacy `export default { schema }` convention).
const SCHEMA_PATH: string | undefined =
  (globalThis as any).process?.env?.ZEROSHIP_SCHEMA_PATH || undefined;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;

// Registry of server functions. Populated by `__register(name, fn)` calls
// that the transform appends to server modules. Importing the user
// module triggers those side-effects.
const registry: Map<string, Function> = new Map();
(globalThis as any).__register = (name: string, fn: Function) => {
  // Last-write-wins so HMR replacements land cleanly.
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
 * Import the user module, retrying once if Vite's dep optimizer regenerated
 * pre-bundled files while our ModuleRunner had a stale version cached.
 *
 * Symptom: `The file does not exist at "…/.vite/deps_zeroship/foo-HASH.js"`.
 * Cause: Vite discovers a new dependency mid-request, re-runs optimizeDeps,
 *        replaces the on-disk bundle with a new content-hash. The Runner's
 *        module graph still references the old hashed URL → 404.
 * Fix:   tear down the Runner and create a fresh one, then retry the import
 *        exactly once. Any further failure is a real error.
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
      const fresh = await getRunner();
      mod = await fresh.import(ENTRY);
    } else {
      throw err;
    }
  }
  await maybeRegisterSchema(mod);
  return mod;
}

// Set once per ModuleRunner lifetime — auto-discovery is idempotent
// (`_installSchema` is re-entrant, but re-running registerModel for
// every RPC dispatch is wasted work). HMR / dep-reoptimize cycles
// reset the flag in lockstep with the runner reset above; a fresh
// runner means a fresh import means re-evaluating the user module's
// `export default { schema }`, so we re-register too.
let schemaRegistered = false;

/**
 * Schema auto-registration in dev mode.
 *
 * Resolution order — mirrors the production synthetic entry exactly
 * (see `sdks/vite-plugin/src/rpc-registry.ts::buildSchemaRegistrationBlock`):
 *
 *   1. `ZEROSHIP_SCHEMA_PATH` env var set → import that module through
 *      the ModuleRunner, use its `default.schema ?? default` as the
 *      schema source. This is the split-file convention.
 *   2. Fall back to `mod.default.schema` (the `export default
 *      { schema }` convention on the entry).
 *   3. Nothing found → no-op.
 *
 * Both resolutions hand the schema to `@zeroship/db::_installSchema`
 * with `installOnEnvDb: true` so `env.db.<collection>` resolves to a
 * typed Collection wrapper before the first RPC dispatch runs.
 *
 * Failures are caught and logged — a malformed schema shouldn't crash
 * the whole dev server.
 */
async function maybeRegisterSchema(mod: any): Promise<void> {
  if (schemaRegistered) return;
  // Load `@zeroship/db` THROUGH the Vite ModuleRunner so the
  // `TypeBuilder` / `SchemaBuilder` classes match the ones the user's
  // module instantiated. Importing the SDK statically from the bundled
  // dev-bootstrap would give us a DIFFERENT class (esbuild's bundled
  // copy vs Vite's loaded copy), and `instanceof` checks inside
  // `_installSchema` would fail with "every field must be a t.*
  // builder" even when the user did call `t.string()`.
  let r: ModuleRunner;
  try {
    r = await getRunner();
  } catch {
    schemaRegistered = true;
    return;
  }

  // Step 1 — split-file path. The env var (set by dev-server based on
  // `resolveSchemaPath`) is the source of truth when present.
  let schema: unknown;
  let provenance: "split-file" | "default-export" | "none" = "none";
  if (SCHEMA_PATH) {
    try {
      const schemaMod: any = await r.import(SCHEMA_PATH);
      // Accept either `{ schema: ... }` (matches the entry-default
      // convention) OR a raw default-exported schema object. Latter
      // form is common when users move the schema to its own file.
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

// Kick off connection immediately
getRunner()
  .then((r) => startHmrPoll(r))
  .catch((e) => console.error("[zeroship:dev] Runner init failed:", e));

/**
 * Poll Vite for changed files every 500ms and invalidate the ModuleRunner's
 * evaluated-module cache for each changed path. This causes the next
 * import() to re-fetch the module from Vite (which re-transforms it).
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
    // capability frame so the fetch isn't refused if it fires inside
    // a query/mutation handler's await window. See transport.ts for
    // the full rationale.
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

// ── Helpers ────────────────────────────────────────────────────────────────

function isAsyncIterator(x: any): boolean {
  return (
    x != null &&
    typeof x === "object" &&
    typeof x[Symbol.asyncIterator] === "function" &&
    typeof x.next === "function"
  );
}

function isParseable(s: any): boolean {
  return s != null && typeof s === "object" && typeof s.parse === "function";
}

function zodIssues(err: any): unknown[] {
  if (err && Array.isArray(err.issues)) return err.issues;
  if (err && Array.isArray(err.errors)) return err.errors;
  return [];
}

function isZodStringSchema(s: any): boolean {
  if (!s || typeof s !== "object") return false;
  const def = s._def || s.def;
  if (!def) return false;
  if (def.typeName === "ZodString") return true;
  if (def.type === "string") return true;
  return false;
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

// ── default.rpc ────────────────────────────────────────────────────────────

/**
 * Mirrors production's `_zsRpc`: looks up the procedure in the registry
 * (populated by the transform's `__register` side-effects when the user
 * module is imported), validates input, runs the handler.
 *
 * Returns a Promise (the user-module import is async) — the kernel
 * routes pending RPC promises through the pump's `settle_rpc_promise`
 * which envelope-wraps the resolved value. Sync-throwing variants
 * become Promise rejections via the `async` wrapper.
 */
async function dispatchRpc(name: string, input: unknown, ctx: unknown): Promise<unknown> {
  await getUserModule();  // populates registry as a side-effect
  const fn = registry.get(name);
  if (typeof fn !== "function") {
    throw Object.assign(new Error(`Method not found: ${name}`), {
      status: 404,
      code: "NOT_FOUND",
    });
  }

  let validated = input;
  const cfg: any = (fn as any).config;
  if (cfg && isParseable(cfg.input)) {
    try {
      validated = cfg.input.parse(input);
    } catch (e) {
      throw Object.assign(new Error("Invalid input"), {
        status: 400,
        code: "INVALID_ARGUMENT",
        details: { issues: zodIssues(e) },
      });
    }
  }

  // B3 capability marker — see rpc-registry.ts comment. Around the user
  // handler invocation we toggle the runtime's CURRENT_KIND so native
  // DB writes (in a query handler) or fetch (in a mutation handler)
  // get refused with a capability_violation envelope. No-op when the
  // natives aren't installed (legacy embeddings).
  const kind = (cfg && typeof cfg.kind === "string" && cfg.kind) ||
               ((fn as any).__zsKind && typeof (fn as any).__zsKind === "string"
                  ? (fn as any).__zsKind
                  : undefined);
  const ek = (typeof globalThis !== "undefined")
    ? (globalThis as any).__zsEnterKind : undefined;
  const xk = (typeof globalThis !== "undefined")
    ? (globalThis as any).__zsExitKind : undefined;
  const tok = (kind && typeof ek === "function") ? ek(kind) : -1;

  // T1 follow-up — Postgres-level auto-tx wrapping. See rpc-registry.ts
  // for the full rationale. query()/mutation() handlers run inside a
  // READ ONLY / SERIALIZABLE Postgres tx; action/stream/subscription
  // bypass. Begin returns 0 if plugin-db decides not to wrap (kind
  // unrelated, or a user-driven `db.transaction(...)` is already
  // open) — End on token 0 is a noop.
  const bt = (typeof globalThis !== "undefined")
    ? (globalThis as any).__zsBeginAutoTx : undefined;
  const et = (typeof globalThis !== "undefined")
    ? (globalThis as any).__zsEndAutoTx : undefined;
  const wantsAutoTx = (kind === "query" || kind === "mutation") &&
                      typeof bt === "function" && typeof et === "function";
  // Per-mutation isolation override (default READ COMMITTED on the
  // native side when empty). See rpc-registry.ts for the rationale.
  const isolation = (cfg && typeof (cfg as any).isolation === "string")
    ? (cfg as any).isolation : "";
  // Cold-start race: under auto-discovery `__zeroshipPlatformReady` is
  // set inside `_installSchema`, which runs inside `maybeRegisterSchema`
  // above. If the first request lands before the dynamic
  // `runner.import("@zeroship/db")` resolves, the platform-ready handle
  // is still undefined and the await below is a no-op — letting an
  // auto-tx open against an unregistered schema. Awaiting
  // `__zsSchemaInit` first pins the happens-before edge.
  const initSchema = getSchemaInit();
  if (initSchema && typeof initSchema.then === "function") {
    try { await initSchema; } catch { /* surfaced via handler */ }
  }
  // Await any platform-readiness promises BEFORE opening the auto-tx.
  // `@zeroship/db`'s `_installSchema` publishes its registerModel chain on
  // `globalThis.__zeroshipPlatformReady`; under pglite-socket's
  // per-connection-in-tx serialization, opening an auto-tx while
  // registerModel still holds `pg_advisory_lock` deadlocks the
  // socket. Awaiting here keeps the two windows disjoint. The await
  // is a no-op on the warm path (chain already settled).
  const ready = getPlatformReady();
  if (ready && typeof ready.then === "function") {
    try { await ready; } catch { /* user-facing errors surface via the handler */ }
  }

  let token = 0;
  if (wantsAutoTx) {
    try { token = await bt(kind, isolation); }
    catch (e) {
      if (tok >= 0 && typeof xk === "function") xk(tok);
      throw e;
    }
  }

  // Two phases:
  //   1. Run the handler. On throw, rollback + rethrow.
  //   2. Commit. On commit failure, surface as caller error.
  // The CURRENT_KIND marker is popped exactly once on every exit
  // path (handler throw, commit throw, success).
  let result: any;
  try {
    const out: any = (fn as any).call(null, validated, ctx);
    result = (out && typeof out.then === "function") ? await out : out;
  } catch (handlerErr) {
    if (wantsAutoTx) {
      try { await et(token, false); } catch (_rb) { /* swallowed */ }
    }
    if (tok >= 0 && typeof xk === "function") xk(tok);
    throw handlerErr;
  }
  if (wantsAutoTx) {
    try { await et(token, true); }
    catch (commitErr) {
      // Commit failure surfaces as the caller-visible error — data
      // integrity wins over the handler's nominal success.
      if (tok >= 0 && typeof xk === "function") xk(tok);
      throw commitErr;
    }
  }
  if (tok >= 0 && typeof xk === "function") xk(tok);

  // Tag async-iterator with output-schema hint for the SSE encoder.
  if (isAsyncIterator(result)) {
    if (cfg && isZodStringSchema(cfg.output)) {
      try { (result as any).__zsOutputIsString = true; } catch (_e) { /* frozen */ }
    }
    return result;
  }

  // Output validation runs in dev only — and dev IS this bootstrap.
  if (cfg && isParseable(cfg.output)) {
    try {
      cfg.output.parse(result);
    } catch (e) {
      throw Object.assign(new Error("Invalid handler output"), {
        status: 500,
        code: "INTERNAL",
        details: { issues: zodIssues(e) },
      });
    }
  }

  return result;
}

// ── default.fetch ──────────────────────────────────────────────────────────

/**
 * WinterCG handler. Owns the /_zs/v1/<id> wire as a fall-through for
 * cases where the kernel's RPC fast path returned an AsyncIterator
 * (the kernel can't encode those inline, so it re-dispatches here for
 * SSE wrapping). Non-/_zs/v1/ paths forward to the user's own
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
  const userDefault = (mod && mod.default && typeof mod.default === "object") ? mod.default : null;
  const userFetch = (userDefault && typeof userDefault.fetch === "function") ? userDefault.fetch : null;
  if (userFetch) return userFetch.call(userDefault, request, env, ctx);
  return new Response("Not Found", { status: 404 });
}

async function rpcAndRespond(name: string, input: unknown, ctx: unknown): Promise<Response> {
  try {
    const result = await dispatchRpc(name, input, ctx);

    if (isAsyncIterator(result)) {
      // AI-SDK Data Stream Protocol — same shape as production.
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

// ── Module entry ──────────────────────────────────────────────────────────

// Same shape the production synthetic entry exports — lets the kernel
// dispatch through default.rpc / default.fetch unchanged across dev vs
// prod.
export default { fetch: dispatchFetch, rpc: dispatchRpc };
