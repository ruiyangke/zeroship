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
  if (!runnerPromise) {
    runnerPromise = createRunner().then((r) => {
      runner = r;
      console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);
      return r;
    });
  }
  return runnerPromise;
}

async function getUserModule(): Promise<any> {
  const r = await getRunner();
  return r.import(ENTRY);
}

// Kick off connection immediately
getRunner().catch((e) => console.error("[zeroship:dev] Runner init failed:", e));

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

    const mod = await getUserModule();
    const fn = mod[rpc.method];

    if (typeof fn !== "function") {
      return jsonResponse({
        jsonrpc: "2.0",
        error: { code: -32601, message: `Method not found: ${rpc.method}` },
        id,
      });
    }

    const result = await fn(...(rpc.params ?? []));
    return jsonResponse({ jsonrpc: "2.0", result, id });
  } catch (e: any) {
    return jsonResponse({
      jsonrpc: "2.0",
      error: { code: -32000, message: e.message ?? String(e) },
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
