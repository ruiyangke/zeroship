import { after, before, describe, test } from "node:test";
import assert from "node:assert/strict";
import { ChildProcess, spawn } from "node:child_process";
import { promises as fs } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { zstdDecompressSync } from "node:zlib";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { createServer, type Plugin, type ViteDevServer } from "vite";

import {
  HMR_FIRST_LOAD_PARAM,
  HMR_POLL_PATH,
  MAX_RAPID_RESTARTS,
  MODULE_FETCH_PATH,
  PROCEDURE_BINDINGS_PATH,
  RUNTIME_MODULE_SPECIFIER,
  VITE_RUNTIME_MODULE_ID,
  DEV_RUNTIME_STATE_HEADER,
  DEV_RUNTIME_FRESH_REQUIRED,
  DEV_RUNTIME_SUPERSEDED,
  RUNTIME_HEALTHY_MS,
  ENV_DEV,
  ENV_DEV_AUTH_SECRET,
  ENV_DEV_PORT,
  ENV_DIE_WITH_PARENT,
  ENV_ENTRY,
  ENV_VITE_ORIGIN,
} from "../src/constants.js";
import { devServerPlugin } from "../src/dev-server.js";
import { zeroshipPlugins } from "../src/plugins.js";
import { DEV_APP_ID } from "../src/gen-types/dev-apply.js";
import { readGeneratedRuntimeDescriptorAt } from "../src/gen-types/read-descriptor.js";
import { createProjectConfigHolder } from "../src/project-config/index.js";
import type { TransformState } from "../src/transform.js";

const __dirname = dirname(fileURLToPath(import.meta.url));
const BOOTSTRAP_SHIM_PATH = resolve(__dirname, "../src/dev-bootstrap.js");
/** The development host the runtime evaluates, as source. */
const DEV_BOOTSTRAP_SOURCE = resolve(__dirname, "../src/dev-bootstrap/index.ts");
const PLUGIN_ROOT = resolve(__dirname, "..");

/** The stub runtime names its own pid on every answer, so a test can tell runtimes apart. */
const STUB_PID_HEADER = "x-stub-runtime-pid";

/** A typed app id that is NOT the shared local one, so the two can be told apart. */
const DECLARED_APP_ID = "app_034klb07lrb9jgma6imvmx000";

/** Every variable the plugin itself sets on the runtime, for a fixture with a
 *  server entry, dev auth on and no `.env`. */
const PLUGIN_RUNTIME_ENV_KEYS = [
  "APP_ID",
  "DATABASE_URL",
  ENV_DEV,
  ENV_DEV_AUTH_SECRET,
  ENV_DIE_WITH_PARENT,
  ENV_ENTRY,
  ENV_VITE_ORIGIN,
];

/** Variables the stub's own `/bin/sh` wrapper may export whatever it inherits. */
const WRAPPER_SHELL_ENV_KEYS = new Set(["OLDPWD", "PWD", "SHLVL", "_"]);

/** The variables the runtime inherited from the plugin, sorted. */
function inheritedFromPlugin(runtime: RuntimeLog): string[] {
  return runtime.envKeys.filter((key) => !WRAPPER_SHELL_ENV_KEYS.has(key));
}

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
    `        ${column}: t.text().required(),`,
    `      },`,
    `    });`,
    `  },`,
    `};`,
    ``,
  ].join("\n");
}

/** The fold on disk, read the way the dev server reads it. The harness's
 *  `zeroship.jsonc` puts the one database's `out` at `generated/zeroship`. */
function foldedDescriptor(root: string): string | undefined {
  return readGeneratedRuntimeDescriptorAt(resolve(root, "generated/zeroship"));
}

/** The published app archive as text. It is a `tar.zst`, so the blobs it
 *  carries - the folded descriptor among them - appear verbatim once the frame
 *  is decompressed. This archive is the channel the fold reaches the runtime
 *  through: the CLI loads it and composes the databases document the runtime
 *  validates from the manifest entries it declares. */
async function publishedArchive(root: string): Promise<string> {
  const { zstdDecompressSync } = await import("node:zlib");
  const archive = await fs.readFile(resolve(root, ".zeroship/app.zship"));
  return zstdDecompressSync(archive).toString();
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
  /** Install the runtime stub at a path relative to the root. */
  writeRuntimeStub: (relativePath: string) => Promise<void>;
}

interface RuntimeLog {
  spawnCount: number;
  pid: number;
  /** The stub file that ran, which tells the project's own bin from a named one. */
  script: string;
  /** Every variable the stub inherited, sorted. */
  envKeys: string[];
  argv: string[];
  env: {
    APP_ID?: string;
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
      failedStartups: [DEV_RUNTIME_FRESH_REQUIRED],
    });
    try {
      const firstRuntime = await harness.runtimeLog();
      const failed = await waitForStatus(`${harness.origin}/api/probe`, 500);
      assert.equal(failed.headers.has(DEV_RUNTIME_STATE_HEADER), false);
      assert.equal(await failed.text(), "module init failed");

      // Nothing has changed since that startup, so a fresh runtime would fail
      // the same way: the failed one keeps answering until the source changes.
      const again = await fetch(`${harness.origin}/api/probe`);
      assert.equal(again.status, 500);
      assert.equal(again.headers.get(STUB_PID_HEADER), String(firstRuntime.pid));
      assert.equal(await harness.runtimeSpawnCount(), 1);

      await harness.queueHmrChange();
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), 2);
        assert.notEqual((await harness.runtimeLog()).pid, firstRuntime.pid);
      });
    } finally {
      await harness.close();
    }
  });

  test("a runtime whose source changed during startup is replaced without an edit", async () => {
    const harness = await startHarness({
      devServerPort: 3920,
      serveRuntime: true,
      failedStartups: [DEV_RUNTIME_SUPERSEDED],
    });
    try {
      const firstRuntime = await harness.runtimeLog();
      const failed = await waitForStatus(`${harness.origin}/api/probe`, 500);
      assert.equal(failed.headers.get(STUB_PID_HEADER), String(firstRuntime.pid));
      assert.equal(failed.headers.has(DEV_RUNTIME_STATE_HEADER), false);

      // No source edit: the change the runtime failed on is already behind it.
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), 2);
        assert.notEqual((await harness.runtimeLog()).pid, firstRuntime.pid);
      });
      const served = await waitForStatus(`${harness.origin}/api/probe`, 200);
      assert.equal(await served.text(), "runtime-ok");
      assert.equal(served.headers.get(STUB_PID_HEADER), String((await harness.runtimeLog()).pid));
    } finally {
      await harness.close();
    }
  });

  test("stops replacing runtimes that are superseded during every startup", async () => {
    const harness = await startHarness({
      devServerPort: 3921,
      serveRuntime: true,
      failedStartups: Array.from({ length: MAX_RAPID_RESTARTS + 2 }, () => DEV_RUNTIME_SUPERSEDED),
    });
    const printed: string[] = [];
    const consoleError = console.error;
    console.error = (...args: unknown[]) => { printed.push(args.map(String).join(" ")); };
    try {
      const stale = resolve(harness.root, "src/stale.ts");
      const changed = resolve(harness.root, "src/value.ts");
      for (let spawned = 1; spawned <= MAX_RAPID_RESTARTS + 1; spawned += 1) {
        await waitFor(async () => {
          assert.equal(await harness.runtimeSpawnCount(), spawned);
        });
        // What the runtime takes from the queue: its first load's own take,
        // which it drops, then a watched file rewritten while that load runs.
        await harness.queueHmrChange(stale);
        await takeChanges(harness, { firstLoad: true });
        await harness.queueHmrChange(changed);
        await takeChanges(harness);
        const failed = await waitForStatus(`${harness.origin}/api/probe`, 500);
        assert.equal(failed.headers.get(STUB_PID_HEADER), String((await harness.runtimeLog()).pid));
      }

      const assertUnsettled = async () => {
        const refused = await fetch(`${harness.origin}/api/probe`);
        assert.equal(refused.status, 503);
        const body = await refused.json() as {
          message?: string;
          retryable?: boolean;
          details?: { state?: string; attempts?: number };
        };
        assert.equal(body.details?.state, "unsettled");
        assert.equal(body.details?.attempts, MAX_RAPID_RESTARTS);
        assert.equal(body.retryable, false);
        assert.match(body.message ?? "", /no longer being replaced/);
        assert.ok(body.message?.includes("src/value.ts"), `message must name the file: ${body.message}`);
        assert.ok(
          !body.message?.includes("src/stale.ts"),
          `a change the first load dropped is not a cause: ${body.message}`,
        );
        assert.ok(!body.message?.includes("bindings"), `the bindings did not change: ${body.message}`);
      };
      await assertUnsettled();
      assert.equal(
        printed.filter((line) => line.includes("dev runtime is no longer being replaced")).length,
        1,
        `the supervisor says why once: ${JSON.stringify(printed)}`,
      );

      // The last runtime stays up past RUNTIME_HEALTHY_MS; that does not lift
      // the verdict and let its bare 500s through.
      await sleep(RUNTIME_HEALTHY_MS + 1_000);
      await assertUnsettled();

      // The last runtime is left in place rather than replaced once more.
      await harness.queueHmrChange(changed);
      assert.equal((await fetch(`${harness.origin}/api/probe`)).status, 503);
      assert.equal(await harness.runtimeSpawnCount(), MAX_RAPID_RESTARTS + 1);

      // An edit to the app spawns afresh with a fresh budget: the next
      // superseded runtime is replaced, not given up on.
      await fs.writeFile(changed, "export const answer = 43;\n");
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), MAX_RAPID_RESTARTS + 2);
      });
      const superseded = await waitForStatus(`${harness.origin}/api/probe`, 500);
      assert.equal(superseded.headers.get(STUB_PID_HEADER), String((await harness.runtimeLog()).pid));
      const served = await waitForStatus(`${harness.origin}/api/probe`, 200);
      assert.equal(await served.text(), "runtime-ok");
      assert.equal(await harness.runtimeSpawnCount(), MAX_RAPID_RESTARTS + 3);
    } finally {
      console.error = consoleError;
      await harness.close();
    }
  });

  test("a runtime that stays up RUNTIME_HEALTHY_MS starts the superseded count over", async () => {
    const harness = await startHarness({
      devServerPort: 3924,
      serveRuntime: true,
      failedStartups: Array.from({ length: MAX_RAPID_RESTARTS + 2 }, () => DEV_RUNTIME_SUPERSEDED),
    });
    try {
      const probeRuntime = async (spawned: number) => {
        await waitFor(async () => {
          assert.equal(await harness.runtimeSpawnCount(), spawned);
        });
        const failed = await waitForStatus(`${harness.origin}/api/probe`, 500);
        assert.equal(failed.headers.get(STUB_PID_HEADER), String((await harness.runtimeLog()).pid));
      };
      // One replacement short of the budget.
      for (let spawned = 1; spawned <= MAX_RAPID_RESTARTS; spawned += 1) await probeRuntime(spawned);

      // This runtime stays up RUNTIME_HEALTHY_MS before anything supersedes it.
      await waitFor(async () => {
        assert.equal(await harness.runtimeSpawnCount(), MAX_RAPID_RESTARTS + 1);
      });
      await sleep(RUNTIME_HEALTHY_MS + 1_000);
      await probeRuntime(MAX_RAPID_RESTARTS + 1);

      // Replaced, and so is the next one, rather than going unsettled.
      await probeRuntime(MAX_RAPID_RESTARTS + 2);
      const served = await waitForStatus(`${harness.origin}/api/probe`, 200);
      assert.equal(await served.text(), "runtime-ok");
      assert.equal(await harness.runtimeSpawnCount(), MAX_RAPID_RESTARTS + 3);
    } finally {
      await harness.close();
    }
  });

  test("closing the server abandons a regeneration whose migration never settles", async () => {
    const harness = await startHarness({
      devServerPort: 3925,
      migrations: { migrationSource: migrationCreating("todos", "title") },
    });
    const printed: string[] = [];
    const consoleWarn = console.warn;
    console.warn = (...args: unknown[]) => { printed.push(args.map(String).join(" ")); };
    try {
      const hanging = resolve(harness.root, "migrations/20240617123100_hangs.ts");
      await fs.writeFile(
        hanging,
        `await new Promise(() => {});\n${migrationCreating("hangs", "body")}`,
      );
      const update = harness.queueHmrChange(hanging);

      const closed = await Promise.race([
        harness.server.close().then(() => "closed"),
        sleep(30_000).then(() => "still waiting"),
      ]);
      assert.equal(closed, "closed", "server.close() waited on a migration that never settles");
      await update;
      assert.ok(
        printed.some((line) => line.includes("regeneration still folding the migrations was abandoned")),
        `the abandonment is reported: ${JSON.stringify(printed)}`,
      );
      const descriptor = JSON.parse(foldedDescriptor(harness.root) ?? "null");
      assert.ok(descriptor?.collections.todos, "the fold from before close is intact");
      assert.equal(descriptor?.collections.hangs, undefined, "the abandoned fold wrote nothing");
    } finally {
      console.warn = consoleWarn;
      await harness.close();
    }
  });

  test("closing the server settles a migration regeneration in flight", async () => {
    const harness = await startHarness({
      devServerPort: 3922,
      migrations: { migrationSource: migrationCreating("todos", "title") },
    });
    const printed: string[] = [];
    const consoleWarn = console.warn;
    console.warn = (...args: unknown[]) => { printed.push(args.map(String).join(" ")); };
    try {
      const migrationFile = resolve(harness.root, "migrations/20240617123100_notes.ts");
      await fs.writeFile(migrationFile, migrationCreating("notes", "body"));
      let settled = false;
      const update = harness.queueHmrChange(migrationFile).then(() => { settled = true; });

      await harness.server.close();
      assert.equal(settled, true, "server.close() resolved while a regeneration was still running");
      await update;
      // Closed mid-fold, the regeneration is abandoned and writes nothing;
      // closed later, it finishes. Either way it is whole, and it is over.
      const descriptor = JSON.parse(foldedDescriptor(harness.root) ?? "null");
      const abandoned = printed.some((line) =>
        line.includes("regeneration still folding the migrations was abandoned"));
      assert.ok(descriptor?.collections.todos, "the boot fold is intact");
      assert.equal(
        Boolean(descriptor?.collections.notes),
        !abandoned,
        `folded notes: ${Boolean(descriptor?.collections.notes)}, reported abandoned: ${abandoned}`,
      );

      // A hot update Vite starts after the server closed folds nothing and
      // reports nothing abandoned: nothing began.
      const reportedBefore = printed.length;
      const lateFile = resolve(harness.root, "migrations/20240617123200_tags.ts");
      await fs.writeFile(lateFile, migrationCreating("tags", "label"));
      await harness.queueHmrChange(lateFile);
      assert.deepEqual(printed.slice(reportedBefore), [], "a late hot update reports nothing");
      const after = JSON.parse(foldedDescriptor(harness.root) ?? "null");
      assert.equal(after?.collections.tags, undefined, "no regeneration after close");
      assert.deepEqual(after, descriptor, "the fold on disk is the one from before close");
    } finally {
      console.warn = consoleWarn;
      await harness.close();
    }
  });

  test("the bootstrap's first load drops changes queued before it and reports later ones", async () => {
    const harness = await startHarness({ devServerPort: 3923 });
    let host: ReturnType<typeof startBootstrapHost> | undefined;
    try {
      // Queued before the host's first load, as the edit that got a runtime
      // spawned is.
      await harness.queueHmrChange();

      host = startBootstrapHost(harness);
      const loaded = await host.next("loaded");
      assert.equal(loaded.fetch, "function", "the first load returns the entry's handlers");

      // The load cleared the queue: nothing stale is left for anyone to take.
      const stale = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.deepEqual((await stale.json() as { changed?: unknown }).changed, []);

      // Control: a change after the load reaches the host's poll.
      await harness.queueHmrChange();
      await host.next("invalidated");
      const exit = await host.stop();
      assert.equal(exit.invalidations, 1, "only the change made after the load invalidated it");
    } finally {
      host?.kill();
      await harness.close();
    }
  });

  test("serves the dev-auth flow while creator runtime startup is broken", async () => {
    const harness = await startHarness({
      devServerPort: 3909,
      serveRuntime: true,
      failedStartups: [DEV_RUNTIME_FRESH_REQUIRED],
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
      // half is crates/zeroship-cli/tests/e2e/parent_death_test.rs.
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

  // WHAT THESE TWO PIN. The dev host runs one process per app and namespaces
  // everything it owns - the app schema, the workflow deployment and
  // activation rows - on the id in APP_ID. Handing every app the same id makes
  // two dev apps on one database indistinguishable to those tables, which
  // surfaces as an activation-identity conflict rather than as a
  // configuration error. The pair differs in ONE variable, the declaration, so
  // the fallback staying put is measured rather than assumed.
  test("spawns the runtime under the app id the workspace declares", async () => {
    const harness = await startHarness({
      devServerPort: 3910,
      declaredAppId: DECLARED_APP_ID,
    });
    try {
      assert.equal((await harness.runtimeLog()).env.APP_ID, DECLARED_APP_ID);
      assert.notEqual(DECLARED_APP_ID, DEV_APP_ID);
    } finally {
      await harness.close();
    }
  });

  test("spawns the runtime under the shared local app id when the workspace declares none", async () => {
    const harness = await startHarness({ devServerPort: 3911 });
    try {
      assert.equal((await harness.runtimeLog()).env.APP_ID, DEV_APP_ID);
    } finally {
      await harness.close();
    }
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

  test("a catalog that broke the startup archive rebuilds it once fixed", async () => {
    // Server code imports a `.po` catalog, which only the app's own plugin
    // loads. The broken catalog fails the startup build, so there is no
    // archive and no runtime; fixing it is the edit that recovers both.
    const catalog: Plugin = {
      name: "fixture:catalog",
      transform(code, id) {
        if (!id.endsWith(".po")) return null;
        const greeting = /^msgstr "(.*)"$/m.exec(code)?.[1];
        if (greeting === undefined) throw new Error("the catalog has no msgstr");
        return { code: `export default ${JSON.stringify({ greeting })};`, map: null, moduleType: "js" };
      },
    };
    let startupFailed!: () => void;
    const failed = new Promise<void>((resolveFailed) => { startupFailed = resolveFailed; });
    const warn = console.warn;
    console.warn = (...args: unknown[]) => {
      warn(...args);
      if (String(args[0]).includes("runtime spawn failed")) startupFailed();
    };
    let harness: Harness | undefined;
    try {
      harness = await startHarness({
        awaitRuntime: false,
        appPlugins: [catalog],
        files: {
          "src/server.ts": [
            "import messages from \"./messages.po\";",
            "export default { fetch() { return new Response(messages.greeting); } };",
            "",
          ].join("\n"),
          "src/messages.po": "msgid \"greeting\"\nmsgstr BROKEN\n",
        },
      });
      await failed;
      const path = resolve(harness.root, ".zeroship/app.zship");
      await assert.rejects(fs.access(path), { code: "ENOENT" });
      await assert.rejects(fs.access(harness.runtimeLogPath), { code: "ENOENT" });

      await fs.writeFile(resolve(harness.root, "src/messages.po"), "msgid \"greeting\"\nmsgstr \"hello from the fixed catalog\"\n");
      await waitFor(async () => {
        assert.match(zstdDecompressSync(await fs.readFile(path)).toString(), /hello from the fixed catalog/);
      });
      await waitFor(async () => {
        assert.equal((await harness!.runtimeLog()).spawnCount, 1);
      });
    } finally {
      console.warn = warn;
      if (harness) {
        const spawned = await fs.access(harness.runtimeLogPath).then(() => true, () => false);
        await harness.close({ expectRuntimeStop: spawned });
      }
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

  test("publishes the in-process generated runtime descriptor to the spawned dev runtime", async () => {
    const harness = await startHarness({
      devServerPort: 3904,
      migrations: { migrationSource: migrationCreating("todos", "title") },
    });
    try {
      const runtime = await harness.runtimeLog();
      const folded = foldedDescriptor(harness.root);
      const descriptor = JSON.parse(folded ?? "null");
      // The in-process gen-types fold produced a valid v2 descriptor with the
      // `todos` collection + its author field (plus injected system fields).
      assert.equal(descriptor?.version, 2, "valid v2 descriptor folded");
      assert.ok(descriptor.collections.todos, "todos collection folded");
      assert.equal(descriptor.collections.todos.fields.title.type, "string", "author field folded");
      assert.ok(descriptor.collections.todos.fields.id, "system id injected");
      // The fold reaches the runtime inside the published app archive, which
      // the CLI composes the databases document from. The collections document
      // is NOT what a runtime is handed, so a child that received this text as
      // an environment variable would be handed a document it refuses.
      assert.ok(
        (await publishedArchive(harness.root)).includes(folded ?? "<unfolded>"),
        "the published archive carries the folded descriptor",
      );
      assert.equal(
        runtime.env.ZEROSHIP_RUNTIME_DESCRIPTOR,
        undefined,
        "the descriptor does not travel in the child's environment",
      );
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
      const folded = foldedDescriptor(harness.root);
      const descriptor = JSON.parse(folded ?? "null");
      assert.equal(descriptor?.version, 2, "the re-fold is a valid v2 descriptor");
      assert.ok(descriptor.collections.todos, "original todos collection retained");
      assert.ok(descriptor.collections.notes, "new notes collection folded in");
      assert.equal(descriptor.collections.notes.fields.body.type, "string", "new author field folded");
      assert.ok(
        (await publishedArchive(harness.root)).includes(folded ?? "<unfolded>"),
        "the archive republished for the fresh child carries the re-fold",
      );

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
        // A child has been spawned, and the archive it is spawned from - each
        // spawn republishes before it spawns - carries both folds, so the
        // update that raced the first spawn was not lost.
        assert.ok((await harness.runtimeLog()).spawnCount >= 1, "a child was spawned");
        const folded = foldedDescriptor(harness.root);
        const descriptor = JSON.parse(folded ?? "null");
        assert.ok(descriptor?.collections.todos, "boot collection retained");
        assert.ok(descriptor?.collections.notes, "racing descriptor update folded");
        assert.ok(
          (await publishedArchive(harness.root)).includes(folded ?? "<unfolded>"),
          "racing descriptor update reached the published archive",
        );
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
      // While restarts are still scheduled, the answer tells the client to retry.
      await waitFor(async () => {
        const response = await fetch(`${harness.origin}/api/probe`);
        const body = await response.json() as {
          retryable?: boolean;
          details?: { state?: string };
        };
        assert.equal(body.details?.state, "failing");
        assert.equal(body.retryable, true);
      });

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

  test("prefers DATABASE_URL from the given environment over .env", async () => {
    // sqlite: values, not postgres:// — `resolveDatabaseUrl` REJECTS a
    // non-SQLite dev URL outright, so a Postgres URL here would never reach
    // the runtime at all. Precedence is what this case tests; the scheme
    // rejection itself is covered directly by test/dev-database-url.test.ts.
    const harness = await startHarness({
      dotenv: "DATABASE_URL=sqlite:.dotenv-dev.sqlite\n",
      processEnv: () => ({ DATABASE_URL: "sqlite:.shell-dev.sqlite" }),
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

  // WHAT THESE TWO PIN. The runtime command, and the environment the runtime
  // inherits, come from the environment the plugin is GIVEN, never from this
  // process's own. Both fixtures hold the same two stubs and differ in ONE
  // variable, whether the given environment names one, so the fallback to the
  // project's own bin is measured rather than assumed. The inherited set is
  // compared whole: a given key must arrive, and nothing else from this
  // process's environment may.
  test("runs the runtime ZEROSHIP_BIN names in the given environment", async () => {
    const harness = await startHarness({
      devServerPort: 3913,
      extraRuntimeStubs: ["named-bin/zeroship"],
      processEnv: (root) => ({ ZEROSHIP_BIN: resolve(root, "named-bin/zeroship") }),
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(runtime.script, resolve(harness.root, "named-bin/zeroship"));
      assert.deepEqual(inheritedFromPlugin(runtime), [...PLUGIN_RUNTIME_ENV_KEYS, "ZEROSHIP_BIN"].sort());
    } finally {
      await harness.close();
    }
  });

  test("runs the project's own runtime when the given environment names none", async () => {
    const harness = await startHarness({
      devServerPort: 3914,
      extraRuntimeStubs: ["named-bin/zeroship"],
    });
    try {
      const runtime = await harness.runtimeLog();
      assert.equal(runtime.script, resolve(harness.root, "node_modules/.bin/zeroship"));
      assert.deepEqual(inheritedFromPlugin(runtime), [...PLUGIN_RUNTIME_ENV_KEYS].sort());
    } finally {
      await harness.close();
    }
  });

  test("takes the runtime port from the given environment when no option names one", async () => {
    const harness = await startHarness({
      devServerPort: null,
      processEnv: () => ({ [ENV_DEV_PORT]: "3915" }),
    });
    try {
      assert.ok((await harness.runtimeLog()).argv.includes("--port=3915"));
    } finally {
      await harness.close();
    }
  });

  // The boot report inspects a database file, and it has to be the one the
  // runtime is told to open. Both are resolved from the given environment.
  test("reports schema state for the database the given environment names", async (t) => {
    const errors = t.mock.method(console, "error");
    const harness = await startHarness({
      devServerPort: 3916,
      migrations: { migrationSource: migrationCreating("todos", "title") },
      processEnv: () => ({ DATABASE_URL: "sqlite:.given-db/dev.sqlite" }),
    });
    try {
      assert.equal((await harness.runtimeLog()).env.DATABASE_URL, "sqlite:.given-db/dev.sqlite");
      const givenDir = resolve(harness.root, ".given-db");
      await waitFor(async () => {
        const printed = errors.mock.calls.map((call) => call.arguments.join(" "));
        assert.ok(
          printed.some((line) => line.includes("dev schema NOT applied") && line.includes(givenDir)),
          `expected a schema report naming ${givenDir}, got ${JSON.stringify(printed)}`,
        );
      });
    } finally {
      await harness.close();
    }
  });

  // WHAT THE NEXT FOUR PIN. A command that cannot be executed reaches the dev
  // server by one of two roads: Node throws from `spawn` itself (ENOTDIR,
  // ENAMETOOLONG, ETXTBSY, an invalid argument), or it reports the failure
  // afterwards on the child's `error` event (ENOENT, EACCES), which unhandled
  // takes Vite down with it. Either way the failure must reach the developer,
  // Vite must keep serving, and requests must be answered HERE - another
  // process holding the runtime port would otherwise answer them.
  test("reports a runtime command that cannot be executed and keeps serving", async (t) => {
    const devServerPort = 3912;
    const decoy = await startDecoy(devServerPort);
    const errors = t.mock.method(console, "error");
    const harness = await startHarness({
      devServerPort,
      awaitRuntime: false,
      processEnv: (root) => ({ ZEROSHIP_BIN: resolve(root, "missing/zeroship") }),
    });
    const missing = resolve(harness.root, "missing/zeroship");
    try {
      await waitForStartFailureLine(() => printedLines(errors), missing);
      await assertAnsweredForUnstartable(harness.origin, missing);
      assert.equal(decoy.hits(), 0, "a request reached the process holding the runtime port");

      const poll = await fetch(`${harness.origin}${HMR_POLL_PATH}`);
      assert.equal(poll.status, 200, "Vite keeps serving after the runtime failed to start");

      // The verdict outlives the window after which a live child is presumed
      // healthy: a child that never existed must not be promoted to one.
      await sleep(RUNTIME_HEALTHY_MS + 500);
      await assertAnsweredForUnstartable(harness.origin, missing);
      assert.equal(decoy.hits(), 0, "a request reached the process holding the runtime port");

      await assert.rejects(
        fs.access(harness.runtimeLogPath),
        "the project's own bin must not stand in for the named command",
      );
    } finally {
      await harness.close({ expectRuntimeStop: false });
      await decoy.close();
    }
  });

  test("reports a runtime command that spawn rejects outright the same way", async (t) => {
    const devServerPort = 3917;
    const decoy = await startDecoy(devServerPort);
    const errors = t.mock.method(console, "error");
    // A path THROUGH the project's bin, which is a file, so `spawn` throws
    // ENOTDIR instead of returning a child.
    const harness = await startHarness({
      devServerPort,
      awaitRuntime: false,
      processEnv: (root) => ({ ZEROSHIP_BIN: resolve(root, "node_modules/.bin/zeroship/sub") }),
    });
    const throughFile = resolve(harness.root, "node_modules/.bin/zeroship/sub");
    try {
      await waitForStartFailureLine(() => printedLines(errors), throughFile);
      await assertAnsweredForUnstartable(harness.origin, throughFile);
      assert.equal(decoy.hits(), 0, "a request reached the process holding the runtime port");
      await assert.rejects(
        fs.access(harness.runtimeLogPath),
        "the project's own bin must not stand in for the named command",
      );
    } finally {
      await harness.close({ expectRuntimeStop: false });
      await decoy.close();
    }
  });

  test("an app change starts the runtime once the missing command exists", async () => {
    const harness = await startHarness({
      devServerPort: 3918,
      awaitRuntime: false,
      serveRuntime: true,
      processEnv: (root) => ({ ZEROSHIP_BIN: resolve(root, "missing/zeroship") }),
    });
    const missing = resolve(harness.root, "missing/zeroship");
    try {
      await waitFor(async () => {
        await assertAnsweredForUnstartable(harness.origin, missing);
      });

      await harness.writeRuntimeStub("missing/zeroship");
      await fs.writeFile(resolve(harness.root, "src/value.ts"), "export const answer = 43;\n");

      await waitFor(async () => {
        const response = await fetch(`${harness.origin}/api/probe`);
        assert.equal(response.status, 200);
        assert.equal(await response.text(), "runtime-ok");
      });
      assert.equal((await harness.runtimeLog()).script, missing);
    } finally {
      await harness.close();
    }
  });

  // A live child emits `error` too, when `kill()` cannot signal it (EPERM).
  // Node's own reporting of that is reproduced on the prototype, because no
  // unprivileged fixture can make a signal to its own child fail.
  test("a runtime that cannot be signalled is not reported as a failed start", async (t) => {
    const errors = t.mock.method(console, "error");
    const harness = await startHarness({ devServerPort: 3919, serveRuntime: true });
    const kill = t.mock.method(ChildProcess.prototype, "kill", function (this: ChildProcess) {
      this.emit("error", Object.assign(new Error("kill EPERM"), { code: "EPERM", syscall: "kill" }));
      return false;
    });
    try {
      await waitForStatus(`${harness.origin}/api/probe`, 200);
      // An app change replaces the child, which starts with a signal to it.
      await fs.writeFile(resolve(harness.root, "src/value.ts"), "export const answer = 43;\n");
      await waitFor(async () => {
        assert.ok(kill.mock.callCount() > 0, "the dev server signalled its runtime");
        assert.ok(
          printedLines(errors).some((line) => line.includes("kill EPERM")),
          "the failed signal is reported",
        );
      });

      assert.equal(
        printedLines(errors).some((line) => line.includes("Failed to start API server")),
        false,
        "a running child was reported as never started",
      );
      const probe = await fetch(`${harness.origin}/api/probe`);
      const body = await probe.json() as { details?: { state?: string } };
      assert.notEqual(body.details?.state, "unstartable");
    } finally {
      kill.mock.restore();
      await harness.close();
    }
  });
});

/** Every line one mocked console method has printed. */
function printedLines(method: { mock: { calls: { arguments: unknown[] }[] } }): string[] {
  return method.mock.calls.map((call) => call.arguments.join(" "));
}

/**
 * A process holding the runtime port, the way a second example's runtime does.
 * Probed once before it is handed back, so a forwarded request is observable
 * rather than silently refused.
 */
async function startDecoy(port: number): Promise<{ hits: () => number; close: () => Promise<void> }> {
  let hits = 0;
  const decoy = http.createServer((_req, res) => {
    hits += 1;
    res.end("another-app");
  });
  await new Promise<void>((resolveListen) => decoy.listen(port, resolveListen));
  assert.equal(await (await fetch(`http://localhost:${port}/`)).text(), "another-app");
  hits = 0;
  return {
    hits: () => hits,
    close: () => new Promise<void>((resolveClose) => decoy.close(() => resolveClose())),
  };
}

/** The terminal line naming the command and the lever that chose it. */
async function waitForStartFailureLine(printed: () => string[], command: string): Promise<void> {
  await waitFor(async () => {
    assert.ok(
      printed().some((line) =>
        line.startsWith("[zeroship] Failed to start API server")
        && line.includes(`${command} (from ZEROSHIP_BIN)`)),
      `expected a start failure naming ${command} (from ZEROSHIP_BIN), got ${JSON.stringify(printed())}`,
    );
  });
}

/** The answer the proxy owes for a runtime command that never started. */
async function assertAnsweredForUnstartable(origin: string, command: string): Promise<void> {
  const probe = await fetch(`${origin}/api/probe`);
  assert.equal(probe.status, 503);
  const body = await probe.json() as {
    code?: string;
    message?: string;
    retryable?: boolean;
    details?: { state?: string };
  };
  assert.equal(body.code, "UNAVAILABLE");
  assert.equal(body.retryable, false);
  assert.equal(body.details?.state, "unstartable");
  // No process ran, so the answer may not speak of attempts or a port.
  assert.match(body.message ?? "", /^zeroship dev runtime could not be started: /);
  assert.ok(
    body.message?.includes(`${command} (from ZEROSHIP_BIN)`),
    `message must name ${command} (from ZEROSHIP_BIN): ${body.message}`,
  );
}

/** Take the queued changes as a runtime does, optionally as its first-load take. */
async function takeChanges(harness: Harness, options: { firstLoad?: boolean } = {}): Promise<unknown> {
  const query = options.firstLoad ? `?${HMR_FIRST_LOAD_PARAM}` : "";
  const resp = await fetch(`${harness.origin}${HMR_POLL_PATH}${query}`);
  assert.equal(resp.status, 200);
  return (await resp.json() as { changed?: unknown }).changed;
}

/**
 * Run the development host from its TypeScript source in a Node child (under
 * tsx, not the V8 runtime), against the harness's real Vite server. What it
 * exercises is the host's own wiring to that server. The child's environment
 * carries the two variables the host reads and nothing else. It prints one
 * event line per step and exits when told to.
 */
function startBootstrapHost(harness: Harness) {
  const script = [
    `const { createDevEntryLoader } = await import(${JSON.stringify(DEV_BOOTSTRAP_SOURCE)});`,
    "const emit = (event) => process.stdout.write(`HOST-EVENT ${JSON.stringify(event)}\\n`);",
    "let invalidations = 0;",
    "const load = createDevEntryLoader(() => {",
    "  invalidations += 1;",
    "  emit({ event: 'invalidated', invalidations });",
    "});",
    "const entry = await load();",
    "emit({ event: 'loaded', fetch: typeof entry.fetch });",
    "process.stdin.on('data', () => {",
    "  emit({ event: 'exit', invalidations });",
    "  process.exit(0);",
    "});",
  ].join("\n");
  const child = spawn(process.execPath, ["--import", "tsx", "--input-type=module", "--eval", script], {
    cwd: PLUGIN_ROOT,
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      [ENV_ENTRY]: harness.serverEntry,
      [ENV_VITE_ORIGIN]: harness.origin,
    },
  });
  const events: Array<Record<string, unknown>> = [];
  let output = "";
  let buffered = "";
  child.stdout!.on("data", (chunk: Buffer) => {
    output += chunk.toString();
    buffered += chunk.toString();
    const lines = buffered.split("\n");
    buffered = lines.pop()!;
    for (const line of lines) {
      if (line.startsWith("HOST-EVENT ")) events.push(JSON.parse(line.slice("HOST-EVENT ".length)));
    }
  });
  child.stderr!.on("data", (chunk: Buffer) => { output += chunk.toString(); });
  const next = async (name: string): Promise<Record<string, unknown>> => {
    let found: Record<string, unknown> | undefined;
    await waitFor(async () => {
      found = events.find((event) => event.event === name);
      assert.ok(found, `the host never reported ${name}; it printed:\n${output}`);
    }, 30_000);
    return found!;
  };
  return {
    next,
    async stop() {
      child.stdin!.write("exit\n");
      return next("exit");
    },
    kill() {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    },
  };
}

async function startHarness(options: {
  dotenv?: string;
  /**
   * The whole process environment the plugin is given, built from the fixture
   * root. The spawned stub inherits it, plus the variables the plugin sets
   * itself and any `.env` entries. An entry absent here is absent for both,
   * whatever this process's own environment holds.
   */
  processEnv?: (root: string) => NodeJS.ProcessEnv;
  /** Copies of the runtime stub beyond the project's own bin, relative to the root. */
  extraRuntimeStubs?: string[];
  /** The app's own plugins, inline beside zeroship's. */
  appPlugins?: Plugin[];
  /** Source files written over the fixture's, relative to the root, before the server starts. */
  files?: Record<string, string>;
  /** Wait for a stub to record its spawn before returning. Off when none can start. */
  awaitRuntime?: boolean;
  /** The `devServerPort` option; `null` passes none, so the environment decides. */
  devServerPort?: number | null;
  /** `apps.app.app` for the fixture's one app. Omitted means the file states none. */
  declaredAppId?: string;
  migrations?: {
    migrationSource: string;
    updateAtListen?: string;
  };
  rapidExitSpawns?: number;
  serveRuntime?: boolean;
  /**
   * The state header each served spawn, in order, answers every request with,
   * as a runtime whose startup failed does: 500 plus `x-zeroship-dev-runtime`.
   * A spawn past the end of the list serves `runtime-ok`.
   */
  failedStartups?: string[];
} = {}): Promise<Harness> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-vite-dev-server-"));
  const serverEntry = resolve(root, "src/server.ts");
  const runtimeLogPath = resolve(root, ".zeroship-runtime.json");
  const runtimeCountPath = resolve(root, ".zeroship-runtime.count");
  const runtimeStopPath = resolve(root, ".zeroship-runtime.stopped");
  const runtimeStubPaths = ["node_modules/.bin/zeroship", ...(options.extraRuntimeStubs ?? [])];

  await fs.mkdir(dirname(serverEntry), { recursive: true });
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
  for (const [path, body] of Object.entries(options.files ?? {})) {
    await fs.mkdir(dirname(resolve(root, path)), { recursive: true });
    await fs.writeFile(resolve(root, path), body);
  }
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
  // A database's migration sources and its fold have NO default: the file is
  // their one holder, so a fixture that wants the dev tier to fold anything has
  // to declare the database. A fixture that wants the runtime spawned under a
  // declared identity has to declare the app id for the same reason, and a
  // migrations directory that does not exist is simply not watched.
  if (options.migrations || options.declaredAppId) {
    await fs.writeFile(
      resolve(root, "zeroship.jsonc"),
      JSON.stringify({
        name: "dev-server-fixture",
        control: "http://localhost:9090",
        runtime_date: "2026-08-14",
        build: { mode: "full", dist: "dist", output: "dist/app.zship" },
        databases: {
          main: {
            id: "dbs_03evr3oqx1200yyd6zj2cebfw",
            migrations: "migrations",
            out: "generated/zeroship",
          },
        },
        apps: {
          app: {
            ...(options.declaredAppId ? { app: options.declaredAppId } : {}),
            databases: ["main"],
            primary: "main",
          },
        },
      }),
    );
  }
  // The stub body is CommonJS run by this process's own node. Every command
  // the plugin can be pointed at is a `/bin/sh` wrapper that names that
  // interpreter absolutely, because the stub inherits only the environment
  // the test hands the plugin, which carries no PATH. The wrapper passes its
  // own path first, so the log says which command ran.
  const runtimeStubBodyPath = resolve(root, ".zeroship-runtime-stub.cjs");
  await fs.writeFile(runtimeStubBodyPath, [
    "const { readFileSync, writeFileSync } = require('node:fs');",
    "const { resolve } = require('node:path');",
    "const root = process.cwd();",
    "const logPath = resolve(root, '.zeroship-runtime.json');",
    "const countPath = resolve(root, '.zeroship-runtime.count');",
    "const stopPath = resolve(root, '.zeroship-runtime.stopped');",
    "const [command, ...args] = process.argv.slice(2);",
    "let spawnCount = 0;",
    "try {",
    "  spawnCount = Number(readFileSync(countPath, 'utf8')) || 0;",
    "} catch {}",
    "spawnCount += 1;",
    "writeFileSync(countPath, String(spawnCount));",
    "writeFileSync(logPath, JSON.stringify({",
    "  spawnCount,",
    "  pid: process.pid,",
    "  script: command,",
    "  envKeys: Object.keys(process.env).sort(),",
    "  argv: args,",
    "  env: {",
    "    APP_ID: process.env.APP_ID,",
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
    `const failedStartups = ${JSON.stringify(options.failedStartups ?? [])};`,
    `const runtimeStateHeader = ${JSON.stringify(DEV_RUNTIME_STATE_HEADER)};`,
    `const pidHeader = ${JSON.stringify(STUB_PID_HEADER)};`,
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
    "  const portArg = args.find((arg) => arg.startsWith('--port='));",
    "  const port = Number(portArg.slice('--port='.length));",
    "  const failedStartup = failedStartups[spawnCount - rapidExitSpawns - 1];",
    "  createServer((_req, res) => {",
    "    res.setHeader(pidHeader, String(process.pid));",
    "    if (failedStartup !== undefined) {",
    "      res.statusCode = 500;",
    "      res.setHeader(runtimeStateHeader, failedStartup);",
    "      res.end('module init failed');",
    "    } else {",
    "      res.end('runtime-ok');",
    "    }",
    "  }).listen(port);",
    "} else {",
    "  setInterval(() => {}, 1000);",
    "}",
    "",
  ].join("\n"));
  const writeRuntimeStub = async (relativePath: string) => {
    const stubPath = resolve(root, relativePath);
    await fs.mkdir(dirname(stubPath), { recursive: true });
    await fs.writeFile(
      stubPath,
      `#!/bin/sh\nexec ${shellQuote(process.execPath)} ${shellQuote(runtimeStubBodyPath)} "$0" "$@"\n`,
      { mode: 0o755 },
    );
  };
  for (const stubPath of runtimeStubPaths) {
    await writeRuntimeStub(stubPath);
  }

  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
  };
  // The build shape now comes from the project config, not from plugin
  // options. A fixture with no migrations writes no zeroship.jsonc, so the
  // holder serves schema defaults; the `config` escape hatch supplies the
  // entry either way, which is the same path a creator with a computed value
  // takes. The project config and the dev server read the SAME environment,
  // as they do under `zeroship()`.
  const processEnv = options.processEnv?.(root) ?? {};
  const devServerPlugins = devServerPlugin(
    {
      devServerPort: options.devServerPort === null ? undefined : options.devServerPort ?? 3901,
      processEnv,
    },
    state,
    createProjectConfigHolder({ override: { build: { serverEntry } } as never, processEnv }),
    // The dev archive's own zeroship plugins, from the same options.
    () => zeroshipPlugins({ config: { build: { serverEntry } } as never }, processEnv),
  );
  const plugins = [...(options.appPlugins ?? []), ...devServerPlugins];
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

    if (options.awaitRuntime ?? true) {
      await waitFor(async () => {
        await fs.access(runtimeLogPath);
      });
    }

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
          await cleanupRoot(root);
        }
      },
      cleanup: async () => cleanupRoot(root),
      queueHmrChange: async (file = serverEntry) => {
        await devServerPluginImpl.hotUpdate!({ file } as any);
      },
      writeRuntimeStub,
    };
  } catch (error) {
    if (server) {
      await server.close().catch(() => {});
    }
    await cleanupRoot(root);
    throw error;
  }
}

/** One POSIX shell word holding `value` verbatim. */
function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'\\''`)}'`;
}

async function cleanupRoot(root: string): Promise<void> {
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
