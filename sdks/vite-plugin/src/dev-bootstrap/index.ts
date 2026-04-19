/**
 * Dev bootstrap — entry module for zeroship V8 runtime in dev mode.
 * Bundled into dist/dev-bootstrap.js by esbuild.
 *
 * Supports both dispatch modes:
 *   1. JSON-RPC on /_rpc — calls exported "use server" functions by name
 *   2. HTTP on all other paths — delegates to onRequest/default export
 *
 * Uses lazy initialization because the V8 runtime may start serving
 * HTTP requests before the ModuleRunner connects to Vite.
 */
import { createRunner } from "./transport";
import type { ModuleRunner } from "vite/module-runner";

const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;

let runner: ModuleRunner | null = null;
let runnerPromise: Promise<ModuleRunner> | null = null;

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
 *
 * Why polling, not WebSocket/SSE:
 * - V8 runtime has no outbound WebSocket client (WS is server-side only)
 * - A streaming fetch would hit the per-request wall timeout
 * - setInterval runs on the pump between requests — no timeout constraints
 * - 500ms latency is acceptable for dev HMR (saves are human-speed)
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

      // Invalidate each changed module so the next import() re-fetches
      for (const file of changed) {
        // The runner tracks modules by their Vite-resolved ID (usually
        // the absolute file path). invalidateModule marks it stale so
        // the next import() calls fetchModule again.
        const mods = runner.evaluatedModules;
        // Try the file path directly and common URL-encoded variants
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
 * Routes:
 *   POST /_rpc  → JSON-RPC dispatch to user's exported functions
 *   *           → user's onRequest or default export
 */
export async function onRequest(req: any): Promise<any> {
  if (!ENTRY) {
    return jsonResponse({ error: "ZEROSHIP_ENTRY not set" }, 500);
  }

  const url = new URL(req.url);

  // ── JSON-RPC dispatch ──────────────────────────────────────────────
  if ((url.pathname === "/_rpc" || url.pathname === "/rpc") && req.method === "POST") {
    return handleRpc(req);
  }

  // ── HTTP dispatch ──────────────────────────────────────────────────
  const mod = await getUserModule();

  if (typeof mod.onRequest === "function") {
    return mod.onRequest(req);
  }
  if (typeof mod.default === "function") {
    return mod.default(req);
  }

  return jsonResponse({ error: `No handler in ${ENTRY}` }, 404);
}

// ── JSON-RPC handler ───────────────────────────────────────────────────

async function handleRpc(req: any): Promise<any> {
  let id = null;
  try {
    const body = typeof req.text === "function" ? await req.text() : String(req.body ?? "");
    const rpc = JSON.parse(body);
    id = rpc.id;

    let mod;
    try {
      mod = await getUserModule();
    } catch (importErr: any) {
      return jsonResponse({
        jsonrpc: "2.0",
        error: { code: -32000, message: `Module import failed: ${importErr.message}`, stack: importErr.stack },
        id,
      });
    }

    const fn = mod[rpc.method];

    if (typeof fn !== "function") {
      return jsonResponse({
        jsonrpc: "2.0",
        error: { code: -32601, message: `Method not found: ${rpc.method}` },
        id,
      });
    }

    const result = await fn(...(rpc.params ?? []));

    // If the function returns a Response (e.g., SSE stream), pass it through
    // directly instead of wrapping in JSON-RPC envelope.
    if (result instanceof Response) {
      return result;
    }

    return jsonResponse({ jsonrpc: "2.0", result, id });
  } catch (e: any) {
    const code = id === null ? -32700 : -32000;
    return jsonResponse({
      jsonrpc: "2.0",
      error: { code, message: e.message ?? String(e) },
      id,
    });
  }
}

function jsonResponse(data: any, status = 200): any {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}
