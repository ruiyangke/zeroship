/**
 * gen-types wiring into @zeroship/vite-plugin — the IN-PROCESS library path.
 *
 * The plugin records `.ts` migrations and folds them into the typed `env.db`
 * surface (`env.db.ts` + `schema.runtime.json`) via the in-process `gen-types`
 * library (`genTypesFromMigrations` — no CLI subprocess or migration-toolchain
 * binary). These tests cover the dev-server integration:
 *
 *  (a) a change under the migrations dir regenerates the artifacts;
 *  (b) an initial regen runs on dev-server boot (configureServer), not only on a
 *      subsequent change;
 *  (c) a change OUTSIDE the migrations dir does NOT regenerate.
 *
 * The unit-level library behaviour (record → genArtifacts → valid v1 + `--check`
 * hard gate) lives in `test/gen-types/{generated,manual}-source.test.ts`.
 *
 * The default output dir stays `generated/zeroship` (committed, not `.zeroship/`)
 * so apps have a stable path to include in tsconfig.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { DEFAULTS } from "../src/project-config/generated.js";
import { devServerPlugin } from "../src/dev-server.js";
import { createProjectConfigHolder } from "../src/project-config/index.js";
import type { TransformState } from "../src/transform.js";

/** A real op.* migration `.ts` (the recorder resolves `@zeroship/migrate`). */
const CREATE_NOTES = `
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_notes",
  up() {
    table("notes").create({
      columns: {
        title: t.text().notNull(),
      },
    });
  },
};
`;

async function makeFixture(
  files: Record<string, string>,
): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `gentypes-dev-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [rel, content] of Object.entries(files)) {
    const abs = resolve(root, rel);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(abs, content);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

type AnyFn = (...a: any[]) => any;

/** Resolve a plugin hook that Vite allows to be either a bare fn or { handler }. */
function hook(plugin: any, name: string): AnyFn {
  const h = plugin?.[name];
  return typeof h === "function" ? h.bind(plugin) : h?.handler?.bind(plugin);
}

function makeServerStub(root: string) {
  return {
    watcher: { add() {} },
    middlewares: { use() {} },
    environments: {},
    config: { root },
    httpServer: { once() {} },
  };
}

/** Poll until `path` exists or the deadline passes (the regen is fire-and-forget). */
async function waitForFile(path: string, timeoutMs = 5000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      await fs.access(path);
      return true;
    } catch {
      if (Date.now() > deadline) return false;
      await new Promise((r) => setTimeout(r, 25));
    }
  }
}

function bootDevServer(root: string) {
  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
  };
  const plugins = devServerPlugin({}, state, createProjectConfigHolder({}));
  const [envPlugin, devPlugin] = plugins as any[];
  hook(envPlugin, "configResolved")({ root, command: "serve" });
  hook(devPlugin, "configureServer")(makeServerStub(root));
  return devPlugin;
}

describe("dev-server → in-process gen-types", () => {
  test("configureServer regenerates env.db.ts on boot (no hotUpdate needed)", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_create_notes.ts": CREATE_NOTES,
    });
    try {
      bootDevServer(fx.root);
      const envDb = join(fx.root, "generated/zeroship/env.db.ts");
      const json = join(fx.root, "generated/zeroship/schema.runtime.json");
      assert.ok(await waitForFile(envDb), "env.db.ts regenerated on boot");
      assert.ok(await waitForFile(json), "schema.runtime.json regenerated on boot");

      const descriptor = JSON.parse(await fs.readFile(json, "utf8"));
      assert.equal(descriptor.version, 1, "valid v1 descriptor");
      assert.ok(descriptor.collections.notes, "notes collection present");
      // System fields injected.
      assert.ok(descriptor.collections.notes.fields.id, "system id injected");
      assert.equal(descriptor.collections.notes.fields.title.type, "string", "author title field");
    } finally {
      await fx.cleanup();
    }
  });

  test("a change under the migrations dir regenerates env.db.ts (in-process)", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_create_notes.ts": CREATE_NOTES,
    });
    try {
      const devPlugin = bootDevServer(fx.root);
      const envDb = join(fx.root, "generated/zeroship/env.db.ts");
      // Let the boot regen land, then wipe the outputs so the hotUpdate assertion
      // measures the hotUpdate branch alone.
      assert.ok(await waitForFile(envDb));
      await fs.rm(join(fx.root, "generated"), { recursive: true, force: true });

      hook(devPlugin, "hotUpdate")({
        file: join(fx.root, "migrations", "20240617123000_create_notes.ts"),
      });
      assert.ok(await waitForFile(envDb), "hotUpdate regenerated env.db.ts");
      await fs.access(join(fx.root, "generated/zeroship/schema.runtime.json"));
    } finally {
      await fx.cleanup();
    }
  });

  test("a change OUTSIDE the migrations dir does NOT regenerate", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_create_notes.ts": CREATE_NOTES,
    });
    try {
      const devPlugin = bootDevServer(fx.root);
      const genDir = join(fx.root, "generated");
      assert.ok(await waitForFile(join(genDir, "zeroship/env.db.ts")));
      await fs.rm(genDir, { recursive: true, force: true });

      // A source file outside migrations → the migration regen branch must not fire.
      hook(devPlugin, "hotUpdate")({ file: join(fx.root, "src", "app.ts") });
      // Give any (erroneous) async regen a chance to write, then assert absence.
      await new Promise((r) => setTimeout(r, 200));
      await assert.rejects(
        () => fs.access(join(genDir, "zeroship/env.db.ts")),
        "no regen for a non-migration change",
      );
    } finally {
      await fx.cleanup();
    }
  });

  // The default now has exactly ONE holder in the whole tree: schema/project-v1.json,
  // from which DEFAULTS is generated. This assertion is what notices if a
  // second one reappears in TypeScript.
  test("the default gen-types output dir is generated/zeroship, held by the schema", () => {
    assert.equal(DEFAULTS["migrations.out"], "generated/zeroship");
  });
});
