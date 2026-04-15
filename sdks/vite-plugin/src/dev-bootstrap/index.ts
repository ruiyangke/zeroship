/**
 * Dev bootstrap — entry module for zeroship V8 runtime in dev mode.
 * Bundled into dist/dev-bootstrap.js by esbuild.
 */
import { createRunner } from "./transport";

const runner = await createRunner();
const ENTRY = (globalThis as any).process?.env?.ZEROSHIP_ENTRY;

if (!ENTRY) {
  throw new Error("[zeroship] ZEROSHIP_ENTRY not set");
}

console.log(`[zeroship:dev] ModuleRunner ready, entry: ${ENTRY}`);

export async function onRequest(req: any): Promise<any> {
  const mod = await runner.import(ENTRY);

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
