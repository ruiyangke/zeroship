/**
 * Creates a ModuleRunner connected to Vite via WebSocket.
 * Runs inside zeroship V8 runtime — uses global WebSocket.
 */
import { ModuleRunner } from "vite/module-runner";
import { zeroshipEvaluator } from "./evaluator";

export async function createRunner(): Promise<ModuleRunner> {
  const wsUrl = (globalThis as any).process?.env?.ZEROSHIP_VITE_WS;
  if (!wsUrl) {
    throw new Error("[zeroship] ZEROSHIP_VITE_WS not set");
  }

  const ws = new WebSocket(wsUrl);

  await new Promise<void>((resolve, reject) => {
    ws.addEventListener("open", () => resolve());
    ws.addEventListener("error", (e: any) => {
      reject(new Error(`[zeroship] WebSocket failed: ${e.message ?? e}`));
    });
  });

  const transport = {
    connect({ onMessage }: { onMessage: (data: any) => void }) {
      ws.addEventListener("message", (event: any) => {
        const parsed = typeof event.data === "string"
          ? JSON.parse(event.data)
          : JSON.parse(event.data.toString());
        onMessage(parsed);
      });
    },
    send(data: any) {
      ws.send(JSON.stringify(data));
    },
  };

  return new ModuleRunner(
    {
      transport,
      hmr: true,
      sourcemapInterceptor: "prepareStackTrace",
    },
    zeroshipEvaluator,
  );
}
