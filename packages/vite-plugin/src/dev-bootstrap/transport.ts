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
import {
  ENV_VITE_ORIGIN,
  MODULE_FETCH_PATH,
} from "../constants.js";

export async function createRunner(): Promise<ModuleRunner> {
  const viteOrigin = (globalThis as any).process?.env?.[ENV_VITE_ORIGIN];
  if (!viteOrigin) {
    throw new Error(`[zeroship] ${ENV_VITE_ORIGIN} not set`);
  }

  // HTTP-based transport. The native dev loader calls ModuleRunner under its
  // anonymous host context, outside creator procedure capability frames.
  const transport = {
    async invoke(data: any): Promise<{ result: any } | { error: any }> {
      try {
        const resp = await fetch(`${viteOrigin}${MODULE_FETCH_PATH}`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(data),
        });
        const json = await resp.json();
        return json;
      } catch (e: any) {
        return { error: { message: e.message ?? String(e) } };
      }
    },
  };

  // hmr=false: ModuleRunner's built-in HMR requires a bidirectional transport.
  // Our HTTP transport can't support it. Instead, HMR is implemented via the
  // poll-based invalidation loop in index.ts (startHmrPoll → invalidateModule).
  return new ModuleRunner(
    {
      transport,
      hmr: false,
    },
    zeroshipEvaluator,
  );
}
