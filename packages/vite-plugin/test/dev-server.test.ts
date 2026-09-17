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
  PROCEDURE_BINDINGS_PATH,
  RUNTIME_MODULE_SPECIFIER,
  VITE_RUNTIME_MODULE_ID,
  DEV_RUNTIME_STATE_HEADER,
  DEV_RUNTIME_FRESH_REQUIRED,
} from "../src/constants.js";
import { devServerPlugin } from "../src/dev-server.js";
import { createProjectConfigHolder } from "../src/project-config/index.js";
import type { TransformState } from "../src/transform.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const BOOTSTRAP_SHIM_PATH = resolve(__dirname, "../src/dev-bootstrap.js");

/** A real op.* migration creating `<table>` with one text `<column>`. The
 *  in-process recorder resolves `@zeroship/migrate` and the fold materialises
 *  the collection + the injected platform system fields. */
function migrationCreating(table: string, column: string): string {
  return [
    `import { table, t } from "@zeroship/migrate";`,
    ``,
    `export default {`,
    `  name: "create_${table}",`,
    `  schema() {`,
    `    table("${table}").create({`,
    `      columns: {`,
    `        ${column}: t.text().notNull(),`,
    `      },`,
    `    });`,
    `  },`,
    `};`,
    ``,
  ].join("\n");
}

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
    ZEROSHIP_DIE_WITH_PARENT?: string;
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

  test("advertises the runtime-owned module to the module runner", async () => {
    const harness = await startHarness();
    try {
      const resp = await fetch(`${harness.origin}${MODULE_FETCH_PATH}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          type: "custom",
          event: "vite:invoke",
          data: {
            id: "builtins-1",
            name: "getBuiltins",
            data: [],
          },
        }),
      });
      assert.equal(resp.status, 200);
      assert.deepEqual(await resp.json(), {
        result: [RUNTIME_MODULE_SPECIFIER, VITE_RUNTIME_MODULE_ID],
      });
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
      const firstPayload = await first.json() as {
        changed?: unknown;
        bindingsVersion?: unknown;
      };
      assert.deepEqual(firstPayload.changed, [harness.serverEntry]);
      assert.equal(typeof firstPayload.bindingsVersion, "string");

      const second = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.equal(second.status, 200);
      const secondPayload = await second.json() as {
        changed?: unknown;
        bindingsVersion?: unknown;
      };
      assert.deepEqual(secondPayload.changed, []);
      assert.equal(secondPayload.bindingsVersion, firstPayload.bindingsVersion);
      assert.equal(await harness.runtimeSpawnCount(), 1, "ordinary HMR keeps the runtime alive");
    } finally {
      await harness.close();
    }
  });

  test("source edits replace a runtime whose initial entry failed", async () => {
    const harness = await startHarness({
      devServerPort: 3908,
      serveRuntime: true,
      freshRuntimeRequired: true,
    });
    try {
      const firstRuntime = await harness.runtimeLog();
      const failed = await waitForStatus(`${harness.origin}/api/probe`, 500);
      assert.equal(failed.headers.has(DEV_RUNTIME_STATE_HEADER), false);
      assert.equal(await failed.text(), "module init failed");

      await harness.queueHmrChange();
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), 2);
        assert.notEqual((await harness.runtimeLog()).pid, firstRuntime.pid);
      });
    } finally {
      await harness.close();
    }
  });

  test("serves the dev-auth flow while creator runtime startup is broken", async () => {
    const harness = await startHarness({
      devServerPort: 3909,
      serveRuntime: true,
      freshRuntimeRequired: true,
    });
    try {
      await waitForStatus(`${harness.origin}/api/probe`, 500);

      const callback = `${harness.origin}/__zeroship/auth/popup-callback`;
      const authorize = await fetch(
        `${harness.origin}/__zeroship/auth/authorize?state=dev-state&redirect_uri=${encodeURIComponent(callback)}`,
      );
      assert.equal(authorize.status, 200);
      const csrfCookie = authorize.headers.get("set-cookie") ?? "";
      const csrf = /__zeroship_dev_csrf=([^;]+)/.exec(csrfCookie)?.[1];
      assert.ok(csrf, "authorize must set the dev CSRF cookie");

      const login = await fetch(`${harness.origin}/__zeroship/auth/authorize`, {
        method: "POST",
        redirect: "manual",
        headers: {
          "content-type": "application/x-www-form-urlencoded",
          cookie: `__zeroship_dev_csrf=${csrf}`,
        },
        body: new URLSearchParams({
          csrf,
          state: "dev-state",
          redirect_uri: callback,
          email: "dev@localhost",
          password: "dev-dev00000",
        }),
      });
      assert.equal(login.status, 302);
      const location = new URL(login.headers.get("location") ?? "", harness.origin);
      const code = location.searchParams.get("code");
      assert.ok(code, "login must mint an authorization code");

      const exchange = await fetch(`${harness.origin}/__zeroship/auth/session`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ code }),
      });
      assert.equal(exchange.status, 200);
      const sessionCookie = exchange.headers.get("set-cookie") ?? "";
      const session = /__zeroship_dev_session=([^;]+)/.exec(sessionCookie)?.[1];
      assert.ok(session, "exchange must set the dev session cookie");

      const probe = await fetch(`${harness.origin}/__zeroship/auth/session`, {
        headers: { cookie: `__zeroship_dev_session=${session}` },
      });
      assert.equal(probe.status, 200);
      assert.equal((await probe.json() as { user?: { email?: string } }).user?.email, "dev@localhost");
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
      // The child is told OUR pid so the kernel can reap it when we die by
      // SIGKILL, which is the one teardown `killChild` can never run. Asserted
      // on the VALUE, not on presence: a wrong pid is worse than an absent one
      // (the runtime treats "recorded parent is not my parent" as "my parent
      // already exited" and refuses to start at all).
      //
      // WHAT THIS DOES NOT PROVE: that anything actually dies. The child here
      // is a node stub that has never heard of PR_SET_PDEATHSIG. The kernel
      // half is crates/zeroship-cli/tests/parent_death_test.rs.
      assert.equal(runtime.env.ZEROSHIP_DIE_WITH_PARENT, String(process.pid));
      assert.deepEqual(runtime.argv, [
        "serve",
        resolve(harness.root, ".zeroship/app.zship"),
        "--port=3901",
        "--workers=1",
        `--dev-bootstrap=${BOOTSTRAP_SHIM_PATH}`,
        "--dev-entry-loader=createDevEntryLoader",
      ]);
      const archive = resolve(harness.root, ".zeroship/app.zship");
      assert.ok((await fs.stat(archive)).size > 0);
    } finally {
      await harness.close();
    }

    assert.equal(process.listenerCount("exit"), beforeExitListeners);
    assert.equal(process.listenerCount("SIGINT"), beforeSigintListeners);
    assert.equal(process.listenerCount("SIGTERM"), beforeSigtermListeners);
  });

  test("restarts the app runtime after publishing changed dependencies", async () => {
    const harness = await startHarness();
    try {
      const before = await harness.runtimeLog();
      const { zstdDecompressSync } = await import("node:zlib");
      const path = resolve(harness.root, ".zeroship/app.zship");
      await fs.writeFile(resolve(harness.root, "src/value.ts"), 'export const answer = "updated-from-dependency";');
      await waitFor(async () => {
        assert.match(zstdDecompressSync(await fs.readFile(path)).toString(), /updated-from-dependency/);
      });
      await waitFor(async () => {
        const after = await harness.runtimeLog();
        assert.notEqual(after.pid, before.pid);
        assert.ok(after.spawnCount > before.spawnCount);
      });
    } finally {
      await harness.close();
    }
  });

  test("runtime recovery keeps the retained workflow archive when current sources do not build", async () => {
    const harness = await startHarness();
    try {
      const path = resolve(harness.root, ".zeroship/app.zship");
      const retained = await fs.readFile(path);
      const before = await harness.runtimeLog();
      await fs.writeFile(resolve(harness.root, "src/value.ts"), 'import "./missing-dependency.js";');
      await harness.triggerUnexpectedExit();
      await waitFor(async () => {
        assert.ok((await harness.runtimeLog()).spawnCount > before.spawnCount);
      });
      assert.deepEqual(await fs.readFile(path), retained);
    } finally {
      await harness.close();
    }
  });

  test("serves versioned procedure bindings to the runtime loader", async () => {
    const harness = await startHarness();
    try {
      const response = await fetch(`${harness.origin}${PROCEDURE_BINDINGS_PATH}`);
      assert.equal(response.status, 200);
      const payload = await response.json() as {
        version?: unknown;
        bindings?: unknown;
      };
      assert.equal(typeof payload.version, "string");
      assert.deepEqual(payload.bindings, []);
    } finally {
      await harness.close();
    }
  });

  test("injects the in-process generated runtime descriptor into the spawned dev runtime", async () => {
    const harness = await startHarness({
      devServerPort: 3904,
      migrations: { migrationSource: migrationCreating("todos", "title") },
    });
    try {
      const runtime = await harness.runtimeLog();
      const descriptor = JSON.parse(runtime.env.ZEROSHIP_RUNTIME_DESCRIPTOR ?? "null");
      // The in-process gen-types fold produced a valid v2 descriptor with the
      // `todos` collection + its author field (plus injected system fields).
      assert.equal(descriptor?.version, 2, "valid v2 descriptor injected");
      assert.ok(descriptor.collections.todos, "todos collection folded");
      assert.equal(descriptor.collections.todos.fields.title.type, "string", "author field folded");
      assert.ok(descriptor.collections.todos.fields.id, "system id injected");
    } finally {
      await harness.close();
    }
  });

  test("migration hot-update regenerates the descriptor and starts a fresh runtime", async () => {
    const harness = await startHarness({
      devServerPort: 3905,
      migrations: { migrationSource: migrationCreating("todos", "title") },
    });
    try {
      const firstRuntime = await harness.runtimeLog();
      assert.equal(firstRuntime.spawnCount, 1);

      // A NEW migration (a second version) adds a `notes` collection. The
      // in-process regen must re-fold the descriptor and replace the child so
      // native boot binds it before the new isolate evaluates app modules.
      const migrationFile = resolve(harness.root, "migrations/20240617123100_notes.ts");
      await fs.writeFile(migrationFile, migrationCreating("notes", "body"));
      await harness.queueHmrChange(migrationFile);

      await waitFor(async () => {
        const runtime = await harness.runtimeLog();
        assert.equal(runtime.spawnCount, 2);
      });

      const runtime = await harness.runtimeLog();
      assert.notEqual(runtime.pid, firstRuntime.pid, "descriptor change replaces the child");
      const descriptor = JSON.parse(runtime.env.ZEROSHIP_RUNTIME_DESCRIPTOR ?? "null");
      assert.equal(descriptor?.version, 2, "fresh child receives a valid v2 descriptor");
      assert.ok(descriptor.collections.todos, "original todos collection retained");
      assert.ok(descriptor.collections.notes, "new notes collection folded in");
      assert.equal(descriptor.collections.notes.fields.body.type, "string", "new author field folded");

      const resp = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.equal(resp.status, 200);
      const payload = await resp.json() as Record<string, unknown>;
      assert.ok(Array.isArray(payload.changed));
      assert.equal(
        Object.hasOwn(payload, "runtimeDescriptorJson"),
        false,
        "descriptor no longer travels over HMR",
      );
    } finally {
      await harness.close();
    }
  });

  test("does not lose a descriptor update racing the initial runtime spawn", async () => {
    const harness = await startHarness({
      devServerPort: 3907,
      migrations: {
        migrationSource: migrationCreating("todos", "title"),
        updateAtListen: migrationCreating("notes", "body"),
      },
    });
    try {
      await waitFor(async () => {
        const runtime = await harness.runtimeLog();
        const descriptor = JSON.parse(runtime.env.ZEROSHIP_RUNTIME_DESCRIPTOR ?? "null");
        assert.ok(descriptor?.collections.todos, "boot collection retained");
        assert.ok(descriptor?.collections.notes, "racing descriptor update reached a child");
      });
    } finally {
      await harness.close();
    }
  });

  test("descriptor correction recovers a supervisor that exhausted the old descriptor", async () => {
    const harness = await startHarness({
      devServerPort: 3906,
      migrations: { migrationSource: migrationCreating("todos", "title") },
      rapidExitSpawns: 4,
      serveRuntime: true,
    });
    try {
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), 4);
        const response = await fetch(`${harness.origin}/api/probe`);
        const body = await response.json() as {
          retryable?: boolean;
          details?: { state?: string };
        };
        assert.equal(body.details?.state, "fatal");
        assert.equal(body.retryable, false);
      }, 15_000);

      const migrationFile = resolve(harness.root, "migrations/20240617123100_notes.ts");
      await fs.writeFile(migrationFile, migrationCreating("notes", "body"));
      await harness.queueHmrChange(migrationFile);

      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), 5);
        const response = await fetch(`${harness.origin}/api/probe`);
        assert.equal(response.status, 200);
        assert.equal(await response.text(), "runtime-ok");
      });
    } finally {
      await harness.close();
    }
  });

  test("prefers DATABASE_URL from the parent environment over .env", async () => {
    // sqlite: values, not postgres:// — `resolveDatabaseUrl` REJECTS a
    // non-SQLite dev URL outright, so a Postgres URL here would never reach
    // the runtime at all. Precedence is what this case tests; the scheme
    // rejection itself is covered directly by test/dev-database-url.test.ts.
    const harness = await startHarness({
      dotenv: "DATABASE_URL=sqlite:.dotenv-dev.sqlite\n",
      parentDatabaseUrl: "sqlite:.shell-dev.sqlite",
      devServerPort: 3902,
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(runtime.env.DATABASE_URL, "sqlite:.shell-dev.sqlite");
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
    migrationSource: string;
    updateAtListen?: string;
  };
  rapidExitSpawns?: number;
  serveRuntime?: boolean;
  freshRuntimeRequired?: boolean;
} = {}): Promise<Harness> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-vite-dev-server-"));
  const serverEntry = resolve(root, "src/server.ts");
  const runtimeLogPath = resolve(root, ".zeroship-runtime.json");
  const runtimeCountPath = resolve(root, ".zeroship-runtime.count");
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
  if (options.migrations) {
    await fs.mkdir(resolve(root, "migrations"), { recursive: true });
    await fs.writeFile(
      resolve(root, "migrations/20240617123000_notes.ts"),
      options.migrations.migrationSource,
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
      "    ZEROSHIP_DIE_WITH_PARENT: process.env.ZEROSHIP_DIE_WITH_PARENT,",
      "  },",
      "}, null, 2));",
      `const rapidExitSpawns = ${options.rapidExitSpawns ?? 0};`,
      `const serveRuntime = ${options.serveRuntime === true};`,
      `const freshRuntimeRequired = ${options.freshRuntimeRequired === true};`,
      `const runtimeStateHeader = ${JSON.stringify(DEV_RUNTIME_STATE_HEADER)};`,
      `const freshRequired = ${JSON.stringify(DEV_RUNTIME_FRESH_REQUIRED)};`,
      "const stop = () => {",
      "  writeFileSync(stopPath, 'stopped');",
      "  process.exit(0);",
      "};",
      "process.on('SIGTERM', stop);",
      "process.on('SIGINT', stop);",
      "process.on('SIGUSR2', () => process.exit(1));",
      "if (spawnCount <= rapidExitSpawns) {",
      "  setTimeout(() => process.exit(1), 20);",
      "} else if (serveRuntime) {",
      "  const { createServer } = require('node:http');",
      "  const portArg = process.argv.find((arg) => arg.startsWith('--port='));",
      "  const port = Number(portArg.slice('--port='.length));",
      "  createServer((_req, res) => {",
      "    if (freshRuntimeRequired) {",
      "      res.statusCode = 500;",
      "      res.setHeader(runtimeStateHeader, freshRequired);",
      "      res.end('module init failed');",
      "    } else {",
      "      res.end('runtime-ok');",
      "    }",
      "  }).listen(port);",
      "} else {",
      "  setInterval(() => {}, 1000);",
      "}",
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
  // The build shape now comes from the project config, not from plugin
  // options. There is no zeroship.jsonc in these fixtures, so the holder
  // serves schema defaults and the `config` escape hatch supplies the entry -
  // which is the same path a creator with a computed value takes.
  const plugins = devServerPlugin(
    { devServerPort: options.devServerPort ?? 3901 },
    state,
    createProjectConfigHolder({ override: { build: { serverEntry } } as never }),
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
    let updateAtListen: Promise<void> | undefined;
    if (options.migrations?.updateAtListen !== undefined) {
      const migrationFile = resolve(root, "migrations/20240617123100_notes.ts");
      updateAtListen = new Promise<void>((resolveUpdate, rejectUpdate) => {
        server!.httpServer?.once("listening", () => {
          fs.writeFile(migrationFile, options.migrations!.updateAtListen!)
            .then(() => devServerPluginImpl.hotUpdate!({ file: migrationFile } as any))
            .then(resolveUpdate, rejectUpdate);
        });
      });
    }
    await server.listen();
    await updateAtListen;

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
        const runtimeAtClose = expectRuntimeStop
          ? await fs.readFile(runtimeLogPath, "utf8")
              .then((json) => JSON.parse(json) as RuntimeLog)
              .catch(() => null)
          : null;
        if (server) {
          await server.close();
          server = null;
        }
        if (expectRuntimeStop) {
          await waitFor(async () => {
            await fs.access(runtimeStopPath);
          });
          if (runtimeAtClose) {
            await waitForProcessExit(runtimeAtClose.pid);
          }
        }
        if (cleanup) {
          await cleanupRoot(root, previousDatabaseUrl);
        }
      },
      cleanup: async () => cleanupRoot(root, previousDatabaseUrl),
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

async function cleanupRoot(
  root: string,
  previousDatabaseUrl: string | undefined,
): Promise<void> {
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

async function waitForStatus(url: string, status: number): Promise<Response> {
  let response: Response | undefined;
  await waitFor(async () => {
    response = await fetch(url);
    assert.equal(response.status, status);
  });
  return response!;
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
