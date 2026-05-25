import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { createServer, type ViteDevServer } from "vite";

import { devServerPlugin } from "../src/dev-server.js";
import type { TransformState } from "../src/transform.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const BOOTSTRAP_SHIM_PATH = resolve(__dirname, "../src/dev-bootstrap.js");

let removeBootstrapShim = false;

interface Harness {
  root: string;
  origin: string;
  server: ViteDevServer;
  serverEntry: string;
  runtimeLogPath: string;
  runtimeStopPath: string;
  runtimeLog: () => Promise<RuntimeLog>;
  close: () => Promise<void>;
  queueHmrChange: (file?: string) => Promise<void>;
}

interface RuntimeLog {
  pid: number;
  argv: string[];
  env: {
    DATABASE_URL?: string;
    ZEROSHIP_DEV?: string;
    ZEROSHIP_ENTRY?: string;
    ZEROSHIP_VITE_WS?: string;
  };
}

before(async () => {
  try {
    await fs.access(BOOTSTRAP_SHIM_PATH);
  } catch {
    removeBootstrapShim = true;
    await fs.writeFile(BOOTSTRAP_SHIM_PATH, "export {};\n");
  }
});

after(async () => {
  if (removeBootstrapShim) {
    await fs.rm(BOOTSTRAP_SHIM_PATH, { force: true });
  }
});

describe("devServerPlugin", () => {
  test("serves module fetch requests through the zeroship environment", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}/__zeroship_fetch`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          type: "custom",
          event: "vite:invoke",
          data: {
            id: "fetch-1",
            name: "fetchModule",
            data: [harness.serverEntry, null, null],
          },
        }),
      });
      assert.equal(resp.status, 200);

      const payload = await resp.json() as { result?: { code?: string; id?: string } };
      assert.equal(typeof payload.result?.code, "string");
      assert.match(payload.result?.code ?? "", /__vite_ssr_|new Response/);
      assert.equal(payload.result?.id, harness.serverEntry);
    } finally {
      await harness.close();
    }
  });

  test("returns and clears pending HMR changes over the poll endpoint", async () => {
    const harness = await startHarness();
    try {
      await harness.queueHmrChange();

      const first = await fetch(`${harness.origin}/__zeroship_hmr_check`);
      assert.equal(first.status, 200);
      assert.deepEqual(await first.json(), {
        changed: [harness.serverEntry],
      });

      const second = await fetch(`${harness.origin}/__zeroship_hmr_check`);
      assert.equal(second.status, 200);
      assert.deepEqual(await second.json(), { changed: [] });
    } finally {
      await harness.close();
    }
  });

  test("spawns the runtime with the default sqlite dev database and tears it down on close", async () => {
    const beforeExitListeners = process.listenerCount("exit");
    const beforeSigintListeners = process.listenerCount("SIGINT");
    const beforeSigtermListeners = process.listenerCount("SIGTERM");

    const harness = await startHarness();
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(runtime.env.DATABASE_URL, "sqlite:.zeroship/dev.sqlite");
      assert.equal(runtime.env.ZEROSHIP_DEV, "1");
      assert.equal(runtime.env.ZEROSHIP_ENTRY, harness.serverEntry);
      assert.match(
        runtime.env.ZEROSHIP_VITE_WS ?? "",
        /^ws:\/\/localhost:\d+\/__zeroship_hmr$/,
      );
      assert.deepEqual(runtime.argv.slice(0, 4), [
        "serve",
        BOOTSTRAP_SHIM_PATH,
        "--port=3901",
        "--workers=1",
      ]);
    } finally {
      await harness.close();
    }

    assert.equal(process.listenerCount("exit"), beforeExitListeners);
    assert.equal(process.listenerCount("SIGINT"), beforeSigintListeners);
    assert.equal(process.listenerCount("SIGTERM"), beforeSigtermListeners);
  });

  test("prefers DATABASE_URL from .env over the parent environment", async () => {
    const harness = await startHarness({
      dotenv: "DATABASE_URL=postgres://dotenv-user:secret@dotenv-host/dotenv-db\n",
      parentDatabaseUrl: "postgres://shell-user:secret@shell-host/shell-db",
      devServerPort: 3902,
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(
        runtime.env.DATABASE_URL,
        "postgres://dotenv-user:secret@dotenv-host/dotenv-db",
      );
    } finally {
      await harness.close();
    }
  });
});

async function startHarness(options: {
  dotenv?: string;
  parentDatabaseUrl?: string;
  devServerPort?: number;
} = {}): Promise<Harness> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-vite-dev-server-"));
  const serverEntry = resolve(root, "src/server.ts");
  const runtimeLogPath = resolve(root, ".zeroship-runtime.json");
  const runtimeStopPath = resolve(root, ".zeroship-runtime.stopped");
  const childScriptPath = resolve(root, "node_modules/.bin/zeroship");
  const previousDatabaseUrl = process.env.DATABASE_URL;

  await fs.mkdir(dirname(serverEntry), { recursive: true });
  await fs.mkdir(dirname(childScriptPath), { recursive: true });
  await fs.writeFile(
    serverEntry,
    [
      "import { answer } from \"./value.ts\";",
      "export default {",
      "  async fetch() {",
      "    return new Response(String(answer));",
      "  },",
      "};",
      "export { answer };",
      "",
    ].join("\n"),
  );
  await fs.writeFile(
    resolve(root, "src/value.ts"),
    "export const answer = 42;\n",
  );
  await fs.writeFile(resolve(root, "index.html"), "<!doctype html><html><body></body></html>\n");
  if (options.dotenv) {
    await fs.writeFile(resolve(root, ".env"), options.dotenv);
  }
  await fs.writeFile(
    childScriptPath,
    [
      "#!/usr/bin/env node",
      "const { writeFileSync } = require('node:fs');",
      "const { resolve } = require('node:path');",
      "const root = process.cwd();",
      "const logPath = resolve(root, '.zeroship-runtime.json');",
      "const stopPath = resolve(root, '.zeroship-runtime.stopped');",
      "writeFileSync(logPath, JSON.stringify({",
      "  pid: process.pid,",
      "  argv: process.argv.slice(2),",
      "  env: {",
      "    DATABASE_URL: process.env.DATABASE_URL,",
      "    ZEROSHIP_DEV: process.env.ZEROSHIP_DEV,",
      "    ZEROSHIP_ENTRY: process.env.ZEROSHIP_ENTRY,",
      "    ZEROSHIP_VITE_WS: process.env.ZEROSHIP_VITE_WS,",
      "  },",
      "}, null, 2));",
      "const stop = () => {",
      "  writeFileSync(stopPath, 'stopped');",
      "  process.exit(0);",
      "};",
      "process.on('SIGTERM', stop);",
      "process.on('SIGINT', stop);",
      "setInterval(() => {}, 1000);",
      "",
    ].join("\n"),
    { mode: 0o755 },
  );

  if (options.parentDatabaseUrl === undefined) {
    delete process.env.DATABASE_URL;
  } else {
    process.env.DATABASE_URL = options.parentDatabaseUrl;
  }

  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
  };
  const plugins = devServerPlugin(
    {
      devServerPort: options.devServerPort ?? 3901,
      serverEntry,
    },
    state,
  );
  const devServerPluginImpl = plugins.find((plugin) => plugin.name === "zeroship:dev-server");
  assert.ok(devServerPluginImpl?.hotUpdate, "expected dev-server plugin with hotUpdate hook");

  let server: ViteDevServer | null = null;
  try {
    server = await createServer({
      configFile: false,
      root,
      logLevel: "silent",
      plugins,
      server: {
        host: "127.0.0.1",
        port: 0,
        strictPort: false,
      },
    });
    await server.listen();

    const addr = server.httpServer?.address();
    assert.ok(addr && typeof addr === "object", "expected Vite HTTP server to listen on a socket");
    const origin = `http://127.0.0.1:${addr.port}`;

    await waitFor(async () => {
      await fs.access(runtimeLogPath);
    });

    return {
      root,
      origin,
      server,
      serverEntry,
      runtimeLogPath,
      runtimeStopPath,
      runtimeLog: async () => JSON.parse(await fs.readFile(runtimeLogPath, "utf8")) as RuntimeLog,
      close: async () => {
        if (server) {
          await server.close();
          server = null;
        }
        await waitFor(async () => {
          await fs.access(runtimeStopPath);
        });
        await cleanupRoot(root, previousDatabaseUrl);
      },
      queueHmrChange: async (file = serverEntry) => {
        await devServerPluginImpl.hotUpdate!({ file } as any);
      },
    };
  } catch (error) {
    if (server) {
      await server.close().catch(() => {});
    }
    await cleanupRoot(root, previousDatabaseUrl);
    throw error;
  }
}

async function cleanupRoot(root: string, previousDatabaseUrl: string | undefined): Promise<void> {
  if (previousDatabaseUrl === undefined) {
    delete process.env.DATABASE_URL;
  } else {
    process.env.DATABASE_URL = previousDatabaseUrl;
  }
  await fs.rm(root, { recursive: true, force: true });
}

async function waitFor(fn: () => Promise<void>, timeoutMs = 10_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (true) {
    try {
      await fn();
      return;
    } catch (error) {
      if (Date.now() >= deadline) {
        throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
}
