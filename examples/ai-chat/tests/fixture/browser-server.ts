// The Playwright web server for the ai-chat suite.
//
// It starts the OpenAI-compatible stub, then the app's own dev server, and
// hands the app the stub's URL and key through the creator-facing
// configuration surface: `ZS_VAR_`-prefixed names become `env.OPENAI_BASE_URL`
// and `env.OPENAI_API_KEY`, which is how a creator points the app at a
// provider. Nothing here reaches for a real OpenAI key or the network.

import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type Server } from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { APP_ORIGIN, GATE_ORIGIN, STUB_API_BASE, STUB_API_KEY, STUB_ORIGIN } from "./settings.ts";
import { startProviderStub } from "./provider-stub.ts";

const appRoot = fileURLToPath(new URL("../..", import.meta.url));
const stubUrl = new URL(STUB_ORIGIN);
const gateUrl = new URL(GATE_ORIGIN);
const stub = await startProviderStub(Number(stubUrl.port), stubUrl.hostname);
console.log(`[fixture] OpenAI-compatible stub listening on ${STUB_ORIGIN}`);

const children = new Set<ChildProcess>();
let closing = false;
let gate: Server | null = null;

function stop(code: number): void {
  if (closing) return;
  closing = true;
  for (const child of children) {
    if (!child.pid) continue;
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
    }
  }
  gate?.close();
  void stub.close().then(() => {
    process.exitCode = code;
  });
}

process.on("SIGTERM", () => stop(0));
process.on("SIGINT", () => stop(0));

const child = spawn("pnpm", ["exec", "vite", "--host", "127.0.0.1"], {
  cwd: appRoot,
  env: {
    ...process.env,
    ZS_VAR_OPENAI_BASE_URL: STUB_API_BASE,
    ZS_VAR_OPENAI_API_KEY: STUB_API_KEY,
  },
  stdio: "inherit",
  detached: true,
});
children.add(child);
child.once("error", (error) => {
  console.error(`[fixture] vite failed to start: ${error.message}`);
  stop(1);
});
child.once("exit", (code) => {
  if (!closing) {
    console.error(`[fixture] vite exited with code ${code}`);
    stop(code ?? 1);
  }
});

// A procedure name the app does not declare: once the runtime answers it with
// a real RPC error the runtime is serving, whereas the dev-server still
// answers 503 while it is not. Probing an unknown name proves readiness
// without invoking the provider.
async function runtimeReady(): Promise<boolean> {
  try {
    const response = await fetch(`${APP_ORIGIN}/__zeroship/v1/__ready`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: '{"json":{}}',
      signal: AbortSignal.timeout(2_000),
    });
    return response.status !== 503;
  } catch {
    return false;
  }
}

const deadline = Date.now() + 90_000;
while (!closing && Date.now() < deadline) {
  if (await runtimeReady()) break;
  await sleep(250);
}
if (closing) process.exit(0);
if (Date.now() >= deadline) {
  console.error("[fixture] the runtime never answered an RPC");
  stop(1);
} else {
  gate = createServer((_, response) => {
    response.writeHead(200).end("ready");
  });
  gate.listen(Number(gateUrl.port), gateUrl.hostname, () =>
    console.log(`[fixture] runtime ready; gate open on ${GATE_ORIGIN}`),
  );
}
