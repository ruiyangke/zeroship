import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { createServer, type ViteDevServer } from "vite";

import {
  ENV_RUNTIME_DESCRIPTOR,
  HMR_POLL_PATH,
  MODULE_FETCH_PATH,
} from "../src/constants.js";
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
  runtimeCountPath: string;
  runtimeStopPath: string;
  runtimeLog: () => Promise<RuntimeLog>;
  runtimeSpawnCount: () => Promise<number>;
  buildEnd: () => void;
  triggerUnexpectedExit: () => Promise<void>;
  close: (options?: { expectRuntimeStop?: boolean; cleanup?: boolean }) => Promise<void>;
  cleanup: () => Promise<void>;
  queueHmrChange: (file?: string) => Promise<void>;
}

interface RuntimeLog {
  spawnCount: number;
  pid: number;
  argv: string[];
  env: {
    DATABASE_URL?: string;
    ZEROSHIP_DEV?: string;
    ZEROSHIP_ENTRY?: string;
    ZEROSHIP_RUNTIME_DESCRIPTOR?: string;
    ZEROSHIP_VITE_ORIGIN?: string;
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
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
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

  test("rejects unknown module-fetch methods", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          type: "custom",
          event: "vite:invoke",
          data: {
            id: "fetch-unknown",
            name: "closeEverything",
            data: [],
          },
        }),
      });
      assert.equal(resp.status, 400);
      assert.deepEqual(await resp.json(), {
        error: { message: "unsupported zeroship fetch method: closeEverything" },
      });
    } finally {
      await harness.close();
    }
  });

  test("bounds request-body buffering for module fetch", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "x".repeat(70 * 1024),
      });
      assert.equal(resp.status, 413);
      assert.deepEqual(await resp.json(), {
        error: { message: "zeroship fetch body exceeds 65536 bytes" },
      });
    } finally {
      await harness.close();
    }
  });

  test("rejects non-JSON module-fetch requests with 415", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "text/plain" },
        body: "{}",
      });
      assert.equal(resp.status, 415);
      assert.deepEqual(await resp.json(), {
        error: {
          message: "zeroship fetch requests must use Content-Type: application/json",
        },
      });
    } finally {
      await harness.close();
    }
  });

  test("rejects empty module-fetch bodies with 400", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      });
      assert.equal(resp.status, 400);
      assert.deepEqual(await resp.json(), {
        error: { message: "zeroship fetch body is empty" },
      });
    } finally {
      await harness.close();
    }
  });

  test("rejects invalid module-fetch JSON with 400", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{not json",
      });
      assert.equal(resp.status, 400);
      assert.deepEqual(await resp.json(), {
        error: { message: "invalid zeroship fetch JSON" },
      });
    } finally {
      await harness.close();
    }
  });

  test("returns and clears pending HMR changes over the poll endpoint", async () => {
    const harness = await startHarness();
    try {
      await harness.queueHmrChange();

      const first = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.equal(first.status, 200);
      assert.deepEqual(await first.json(), {
        changed: [harness.serverEntry],
      });

      const second = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
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
        runtime.env.ZEROSHIP_VITE_ORIGIN ?? "",
        /^http:\/\/localhost:\d+$/,
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

  test("injects the generated runtime descriptor into the spawned dev runtime", async () => {
    const descriptor = JSON.stringify({
      version: 1,
      collections: {
        todos: {
          fields: { title: { type: "string" } },
          options: { softDelete: false, versioning: false },
          indexes: [],
        },
      },
    });
    const harness = await startHarness({
      devServerPort: 3904,
      migrations: { descriptorJson: descriptor },
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.deepEqual(
        JSON.parse(runtime.env.ZEROSHIP_RUNTIME_DESCRIPTOR ?? "null"),
        JSON.parse(descriptor),
      );
    } finally {
      await harness.close();
    }
  });

  test("migration hot-update regenerates and re-injects the runtime descriptor", async () => {
    const firstDescriptor = JSON.stringify({
      version: 1,
      collections: {
        todos: {
          fields: { title: { type: "string" } },
          options: { softDelete: false, versioning: false },
          indexes: [],
        },
      },
    });
    const secondDescriptor = JSON.stringify({
      version: 1,
      collections: {
        notes: {
          fields: { body: { type: "string" } },
          options: { softDelete: true, versioning: false },
          indexes: [],
        },
      },
    });
    const harness = await startHarness({
      devServerPort: 3905,
      migrations: { descriptorJson: firstDescriptor },
    });
    try {
      const migrationFile = resolve(harness.root, "migrations/20240617123000_notes.ts");
      process.env.ZSTUB_DESCRIPTOR = secondDescriptor;
      await fs.writeFile(migrationFile, "export function up() { return 'changed'; }\n");
      await harness.queueHmrChange(migrationFile);

      const resp = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.equal(resp.status, 200);
      const payload = await resp.json() as {
        changed?: string[];
        runtimeDescriptorJson?: string | null;
      };
      assert.ok(
        payload.changed?.includes(migrationFile),
        `expected HMR payload to include ${migrationFile}`,
      );
      assert.deepEqual(
        JSON.parse(payload.runtimeDescriptorJson ?? "null"),
        JSON.parse(secondDescriptor),
      );
    } finally {
      await harness.close();
    }
  });

  test("prefers DATABASE_URL from the parent environment over .env", async () => {
    const harness = await startHarness({
      dotenv: "DATABASE_URL=postgres://dotenv-user:secret@dotenv-host/dotenv-db\n",
      parentDatabaseUrl: "postgres://shell-user:secret@shell-host/shell-db",
      devServerPort: 3902,
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(
        runtime.env.DATABASE_URL,
        "postgres://shell-user:secret@shell-host/shell-db",
      );
    } finally {
      await harness.close();
    }
  });

  test("buildEnd cancels a pending crash restart before it can respawn", async () => {
    const harness = await startHarness({
      devServerPort: 3903,
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(runtime.spawnCount, 1);

      await harness.triggerUnexpectedExit();
      await waitForProcessExit(runtime.pid);

      harness.buildEnd();
      await sleep(1_200);

      assert.equal(await harness.runtimeSpawnCount(), 1);
    } finally {
      await harness.close({ expectRuntimeStop: false });
    }
  });
});

async function startHarness(options: {
  dotenv?: string;
  parentDatabaseUrl?: string;
  devServerPort?: number;
  migrations?: {
    descriptorJson: string;
  };
} = {}): Promise<Harness> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-vite-dev-server-"));
  const serverEntry = resolve(root, "src/server.ts");
  const runtimeLogPath = resolve(root, ".zeroship-runtime.json");
  const runtimeCountPath = resolve(root, ".zeroship-runtime.count");
  const runtimeStopPath = resolve(root, ".zeroship-runtime.stopped");
  const childScriptPath = resolve(root, "node_modules/.bin/zeroship");
  const migrateCliPath = resolve(root, "node_modules/.bin/zeroship-migrate-js");
  const previousDatabaseUrl = process.env.DATABASE_URL;
  const previousStubDescriptor = process.env.ZSTUB_DESCRIPTOR;

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
  if (options.migrations) {
    process.env.ZSTUB_DESCRIPTOR = options.migrations.descriptorJson;
    await fs.mkdir(resolve(root, "migrations"), { recursive: true });
    await fs.writeFile(
      resolve(root, "migrations/20240617123000_notes.ts"),
      "export function up() {}\n",
    );
    await fs.writeFile(
      migrateCliPath,
      [
        "#!/usr/bin/env node",
        "const fs = require('node:fs');",
        "const path = require('node:path');",
        "const args = process.argv.slice(2);",
        "if (args[0] !== 'gen-types') { process.stderr.write('bad command'); process.exit(2); }",
        "const out = args[args.indexOf('--out') + 1];",
        "fs.mkdirSync(out, { recursive: true });",
        "fs.writeFileSync(path.join(out, 'env.db.ts'), '// stub env.db.ts\\n');",
        "fs.writeFileSync(path.join(out, 'schema.runtime.json'), process.env.ZSTUB_DESCRIPTOR + '\\n');",
        "",
      ].join("\n"),
      { mode: 0o755 },
    );
  }
  await fs.writeFile(
    childScriptPath,
    [
      "#!/usr/bin/env node",
      "const { readFileSync, writeFileSync } = require('node:fs');",
      "const { resolve } = require('node:path');",
      "const root = process.cwd();",
      "const logPath = resolve(root, '.zeroship-runtime.json');",
      "const countPath = resolve(root, '.zeroship-runtime.count');",
      "const stopPath = resolve(root, '.zeroship-runtime.stopped');",
      "let spawnCount = 0;",
      "try {",
      "  spawnCount = Number(readFileSync(countPath, 'utf8')) || 0;",
      "} catch {}",
      "spawnCount += 1;",
      "writeFileSync(countPath, String(spawnCount));",
      "writeFileSync(logPath, JSON.stringify({",
      "  spawnCount,",
      "  pid: process.pid,",
      "  argv: process.argv.slice(2),",
      "  env: {",
      "    DATABASE_URL: process.env.DATABASE_URL,",
      "    ZEROSHIP_DEV: process.env.ZEROSHIP_DEV,",
      "    ZEROSHIP_ENTRY: process.env.ZEROSHIP_ENTRY,",
      "    ZEROSHIP_RUNTIME_DESCRIPTOR: process.env.ZEROSHIP_RUNTIME_DESCRIPTOR,",
      "    ZEROSHIP_VITE_ORIGIN: process.env.ZEROSHIP_VITE_ORIGIN,",
      "  },",
      "}, null, 2));",
      "const stop = () => {",
      "  writeFileSync(stopPath, 'stopped');",
      "  process.exit(0);",
      "};",
      "process.on('SIGTERM', stop);",
      "process.on('SIGINT', stop);",
      "process.on('SIGUSR2', () => process.exit(1));",
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
      ...(options.migrations
        ? {
            migrations: {
              cliPath: migrateCliPath,
              dir: "migrations",
              genTypesOut: "generated/zeroship",
            },
          }
        : {}),
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
      runtimeCountPath,
      runtimeStopPath,
      runtimeLog: async () => JSON.parse(await fs.readFile(runtimeLogPath, "utf8")) as RuntimeLog,
      runtimeSpawnCount: async () => Number(await fs.readFile(runtimeCountPath, "utf8")),
      buildEnd: () => {
        devServerPluginImpl.buildEnd?.call(devServerPluginImpl);
      },
      triggerUnexpectedExit: async () => {
        const runtime = JSON.parse(await fs.readFile(runtimeLogPath, "utf8")) as RuntimeLog;
        process.kill(runtime.pid, "SIGUSR2");
      },
      close: async (closeOptions = {}) => {
        const { expectRuntimeStop = true, cleanup = true } = closeOptions;
        if (server) {
          await server.close();
          server = null;
        }
        if (expectRuntimeStop) {
          await waitFor(async () => {
            await fs.access(runtimeStopPath);
          });
        }
        if (cleanup) {
          await cleanupRoot(root, previousDatabaseUrl, previousStubDescriptor);
        }
      },
      cleanup: async () => cleanupRoot(root, previousDatabaseUrl, previousStubDescriptor),
      queueHmrChange: async (file = serverEntry) => {
        await devServerPluginImpl.hotUpdate!({ file } as any);
      },
    };
  } catch (error) {
    if (server) {
      await server.close().catch(() => {});
    }
    await cleanupRoot(root, previousDatabaseUrl, previousStubDescriptor);
    throw error;
  }
}

async function cleanupRoot(
  root: string,
  previousDatabaseUrl: string | undefined,
  previousStubDescriptor: string | undefined,
): Promise<void> {
  if (previousDatabaseUrl === undefined) {
    delete process.env.DATABASE_URL;
  } else {
    process.env.DATABASE_URL = previousDatabaseUrl;
  }
  if (previousStubDescriptor === undefined) {
    delete process.env.ZSTUB_DESCRIPTOR;
  } else {
    process.env.ZSTUB_DESCRIPTOR = previousStubDescriptor;
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

async function waitForProcessExit(pid: number, timeoutMs = 10_000): Promise<void> {
  await waitFor(async () => {
    try {
      process.kill(pid, 0);
      throw new Error(`process ${pid} is still alive`);
    } catch (error: any) {
      if (error?.code !== "ESRCH") {
        throw error;
      }
    }
  }, timeoutMs);
}

async function sleep(ms: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}
