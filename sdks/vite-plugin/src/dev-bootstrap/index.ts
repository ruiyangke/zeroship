/**
 * Dev bootstrap — entry module for zeroship V8 runtime in dev mode.
 * Bundled into dist/dev-bootstrap.js by esbuild.
 *
 * Dispatch wire (v2):
 *   POST /_rpc/<modulePath>/<exportName>   → path-based RPC with JSON args array
 *   *                                       → user's onRequest or default export
 *
 * The registry is populated on the server side by `__register(name, fn)`
 * side-effects that the vite-plugin transform appends to each "use server"
 * module. Importing the user entry runs those side-effects.
 */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";

const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;

// Registry of server functions. Populated by `__register(name, fn)` calls
// that the transform appends to server modules. Exposed as a global so
// every transformed module can push into it (ModuleRunner shares the
// runner's own global scope with the V8 isolate).
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
  try {
    return await r.import(ENTRY);
  } catch (err: any) {
    const msg = String(err?.message ?? err ?? "");
    if (msg.includes("is in the optimize deps directory")) {
      console.log("[zeroship:dev] deps re-optimized, resetting runner");
      runner = null;
      runnerPromise = null;
      const fresh = await getRunner();
      return fresh.import(ENTRY);
    }
    throw err;
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
    }
  }, 500);
}

/**
 * HTTP request handler — called by the Rust runtime for every request.
 *
 * POST /_rpc/<methodName>   → registry lookup + invoke
 * *                          → user's onRequest or default export
 */
export async function onRequest(req: any): Promise<any> {
  if (!ENTRY) {
    return errorResponse("ZEROSHIP_ENTRY not set", 500);
  }

  const url = new URL(req.url);

  // ── URL-path RPC ────────────────────────────────────────────────────
  if (url.pathname.startsWith("/_rpc/") && req.method === "POST") {
    return handleRpcPath(url.pathname.slice("/_rpc/".length), req);
  }

  // Legacy /_rpc and /rpc (JSON-RPC envelope) — surface migration error.
  if ((url.pathname === "/_rpc" || url.pathname === "/rpc") && req.method === "POST") {
    return errorResponse(
      "The JSON-RPC envelope is gone. Use POST /_rpc/<methodName> with a JSON array body.",
      410,
    );
  }

  // ── HTTP dispatch ──────────────────────────────────────────────────
  const mod = await getUserModule();

  if (typeof mod.onRequest === "function") {
    return mod.onRequest(req);
  }
  if (typeof mod.default === "function") {
    return mod.default(req);
  }

  return errorResponse(`No handler in ${ENTRY}`, 404);
}

/**
 * Dispatch a URL-path RPC call. Loads the user module (which populates
 * the registry as a side effect), looks up the method, parses args, invokes.
 *
 * Return shapes:
 *   - unary: Response(JSON.stringify(value), 200, application/json)
 *   - stream (async generator): Response(ReadableStream) with SSE frames
 *   - throw: Response(JSON error body, status from err.status or 500)
 *   - user-returned Response: passthrough
 */
async function handleRpcPath(methodName: string, req: any): Promise<any> {
  // Touch the user module so its __register side-effects populate the
  // registry. We do this every request — getUserModule() hits the
  // ModuleRunner's evaluated cache, so after warmup it's effectively
  // free; on HMR invalidation the registry is repopulated with the
  // fresh function references.
  try {
    await getUserModule();
  } catch (importErr: any) {
    return errorResponse(`Module import failed: ${importErr.message}`, 500, importErr);
  }

  const fn = registry.get(methodName);
  if (typeof fn !== "function") {
    return errorResponse(`Method not found: ${methodName}`, 404);
  }

  let args: any[] = [];
  try {
    const body = typeof req.text === "function" ? await req.text() : String(req.body ?? "");
    if (body) {
      const parsed = JSON.parse(body);
      if (parsed != null) {
        if (!Array.isArray(parsed)) {
          return errorResponse("RPC args body must be a JSON array", 400);
        }
        args = parsed;
      }
    }
  } catch (e: any) {
    return errorResponse(`Invalid args JSON: ${e.message ?? String(e)}`, 400);
  }

  let result: any;
  try {
    result = fn.apply(null, args);
  } catch (e: any) {
    return errorResponse(e?.message ?? String(e), statusFromError(e), e);
  }

  // Async generator → SSE stream
  if (
    result != null && typeof result === "object" &&
    typeof result[Symbol.asyncIterator] === "function" &&
    typeof result.next === "function" &&
    typeof result.return === "function"
  ) {
    return wrapAsyncGenerator(result);
  }

  // Promise path
  if (result && typeof result.then === "function") {
    try {
      result = await result;
    } catch (e: any) {
      return errorResponse(e?.message ?? String(e), statusFromError(e), e);
    }

    // Resolved to an async generator (rare — unwrap)
    if (
      result != null && typeof result === "object" &&
      typeof result[Symbol.asyncIterator] === "function" &&
      typeof result.next === "function" &&
      typeof result.return === "function"
    ) {
      return wrapAsyncGenerator(result);
    }
  }

  // User-returned Response (e.g. form B streaming): passthrough
  if (result instanceof Response) {
    return result;
  }

  // Plain value → JSON body
  return new Response(JSON.stringify(result === undefined ? null : result), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

function wrapAsyncGenerator(gen: any): any {
  const encoder = new TextEncoder();
  const body = new ReadableStream({
    async start(controller) {
      try {
        while (true) {
          const step = await gen.next();
          if (step.done) {
            const retJson = JSON.stringify(step.value === undefined ? null : step.value);
            controller.enqueue(encoder.encode(`event: return\ndata: ${retJson}\n\n`));
            break;
          }
          const valJson = JSON.stringify(step.value === undefined ? null : step.value);
          controller.enqueue(encoder.encode(`event: yield\ndata: ${valJson}\n\n`));
        }
      } catch (e: any) {
        const payload = JSON.stringify({
          message: e?.message ?? String(e),
          name: e?.name ?? "Error",
        });
        controller.enqueue(encoder.encode(`event: error\ndata: ${payload}\n\n`));
      } finally {
        controller.close();
      }
    },
  });
  return new Response(body, {
    status: 200,
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache, no-transform",
      "X-Accel-Buffering": "no",
    },
  });
}

function statusFromError(e: any): number {
  const s = e?.status;
  if (typeof s === "number" && s >= 400 && s < 600) return s;
  return 500;
}

function errorResponse(message: string, status: number, err?: any): any {
  const body: Record<string, unknown> = { message, name: err?.name ?? "Error" };
  if (err?.stack) body.stack = err.stack;
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}
