/**
 * Creates a ModuleRunner connected to Vite via HTTP fetch.
 * Runs inside zeroship V8 runtime — uses global fetch().
 *
 * The runtime doesn't support outbound WebSocket connections (WebSocket is
 * server-side only, via WebSocketPair). So we use HTTP-based transport:
 * - invoke() uses fetch() to call Vite's fetchModule endpoint
 * - HMR is enabled: module graph invalidation is handled by the runner
 */
import { ModuleRunner } from "vite/module-runner";
import { zeroshipEvaluator } from "./evaluator";

export async function createRunner(): Promise<ModuleRunner> {
  const viteWsUrl = (globalThis as any).process?.env?.ZEROSHIP_VITE_WS;
  if (!viteWsUrl) {
    throw new Error("[zeroship] ZEROSHIP_VITE_WS not set");
  }

  // Convert ws://localhost:5199/__zeroship_hmr → http://localhost:5199
  const viteOrigin = viteWsUrl
    .replace(/^ws:/, "http:")
    .replace(/^wss:/, "https:")
    .replace(/\/__zeroship_hmr$/, "");

  // HTTP-based transport: uses fetch for module requests.
  const transport = {
    async invoke(data: any): Promise<{ result: any } | { error: any }> {
      try {
        const resp = await fetch(`${viteOrigin}/__zeroship_fetch`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(data),
        });
        const json = JSON.parse(await resp.text());
        return json;
      } catch (e: any) {
        return { error: { message: e.message ?? String(e) } };
      }
    },
  };

  return new ModuleRunner(
    {
      transport,
      hmr: true,
    },
    zeroshipEvaluator,
  );
}
