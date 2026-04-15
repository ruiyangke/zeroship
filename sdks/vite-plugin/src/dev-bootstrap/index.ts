/**
 * Dev bootstrap — entry module for zeroship V8 runtime in dev mode.
 * Bundled into dist/dev-bootstrap.js by esbuild.
 *
 * Uses lazy initialization instead of top-level await because the V8 runtime
 * may start serving HTTP requests before TLA promises resolve.
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

// Kick off connection immediately (don't await — just start it)
getRunner().catch((e) => console.error("[zeroship:dev] Runner init failed:", e));

export async function onRequest(req: any): Promise<any> {
  if (!ENTRY) {
    return new Response(
      JSON.stringify({ error: "ZEROSHIP_ENTRY not set" }),
      { status: 500, headers: { "Content-Type": "application/json" } },
    );
  }

  const r = await getRunner();
  const mod = await r.import(ENTRY);

  if (typeof mod.onRequest === "function") {
    return mod.onRequest(req);
  }
  if (typeof mod.default === "function") {
    return mod.default(req);
  }

  return new Response(
    JSON.stringify({ error: `No onRequest or default handler in ${ENTRY}` }),
    { status: 404, headers: { "Content-Type": "application/json" } },
  );
}
