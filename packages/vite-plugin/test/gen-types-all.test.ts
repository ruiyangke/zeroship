/**
 * The repo-wide gen-types runner reads EACH app's own `zeroship.jsonc`.
 *
 * The runner walks a whole workspace and regenerates every artifact directory
 * it finds. `ZEROSHIP_CONFIG` names one config file for a single build, so a
 * runner that honored it would point every app in the walk at that one file.
 * The check below stands a workspace of two apps with different databases and
 * out dirs, sets `ZEROSHIP_CONFIG` to app A's config on the child's
 * environment, and asserts app B's artifacts still come from app B's file.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";

import { ENV_DB_FILE, RUNTIME_DESCRIPTOR_FILE } from "../src/gen-types/index.js";
import { GENERATED_ENV_DB_BANNER } from "../src/gen-types/render-env-db.js";
import { CONFIG_ENV_VAR } from "../src/project-config/index.js";

const PACKAGE_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const RUNNER = resolve(PACKAGE_ROOT, "scripts/gen-types-all.ts");

/** One app: its own database label, out dir and migration table. */
interface AppSpec {
  /** Directory name under the workspace root. */
  dir: string;
  /** Database label declared in this app's `zeroship.jsonc`. */
  database: string;
  /** App label declared in this app's `zeroship.jsonc`. */
  app: string;
  /** Directory the `databases.*.out` names, relative to the app root. */
  out: string;
  /** The table its migration creates, unique per app. */
  table: string;
}

const APP_A: AppSpec = { dir: "app-a", database: "alpha", app: "a_store", out: "gen/alpha", table: "notes_a" };
const APP_B: AppSpec = { dir: "app-b", database: "beta", app: "b_store", out: "gen/beta", table: "notes_b" };

function configFor(spec: AppSpec): string {
  return JSON.stringify(
    {
      name: spec.dir,
      control: "http://localhost:9090",
      runtime_date: "2026-08-14",
      build: { mode: "full", dist: "dist", output: "dist/app.zship" },
      databases: {
        [spec.database]: {
          id: `dbs_${(spec.database === "alpha" ? "a" : "b").repeat(25)}`,
          migrations: "migrations",
          out: spec.out,
        },
      },
      apps: { [spec.app]: { databases: [spec.database], primary: spec.database } },
    },
    null,
    2,
  );
}

function migrationFor(spec: AppSpec): string {
  return `
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_${spec.table}",
  schema() {
    table("${spec.table}").create({
      columns: { title: t.text().required() },
    });
  },
};
`;
}

async function makeWorkspace(): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `gentypes-all-${randomUUID()}`);
  for (const spec of [APP_A, APP_B]) {
    const appRoot = join(root, spec.dir);
    const outDir = join(appRoot, spec.out);
    await fs.mkdir(join(appRoot, "migrations"), { recursive: true });
    await fs.mkdir(outDir, { recursive: true });
    await fs.writeFile(join(appRoot, "package.json"), JSON.stringify({ name: spec.dir, private: true }));
    await fs.writeFile(join(appRoot, "zeroship.jsonc"), configFor(spec));
    await fs.writeFile(join(appRoot, "migrations", `20260101000000_create_${spec.table}.ts`), migrationFor(spec));
    // Discovery finds an artifact directory by the two committed filenames,
    // and the emitter only overwrites files it recognises as its own output.
    await fs.writeFile(join(outDir, ENV_DB_FILE), GENERATED_ENV_DB_BANNER);
    await fs.writeFile(join(outDir, RUNTIME_DESCRIPTOR_FILE), '{"version":1,"collections":{}}\n');
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

/** Run the runner through a child whose own environment names app A's config. */
async function runWithRedirectedConfig(root: string, configPath: string): Promise<string> {
  const probe = join(root, "probe.mjs");
  const body = [
    `import { regenerateAll } from ${JSON.stringify(pathToFileURL(RUNNER).href)};`,
    "try {",
    `  await regenerateAll(${JSON.stringify(root)}, false);`,
    "  console.log('regen-ok');",
    "} catch (error) {",
    "  console.log('regen-failed: ' + error.message);",
    "}",
    "",
  ].join("\n");
  // The write happens in a disposable child, so this process's environment is
  // never touched; only the child is told to read app A's config.
  await fs.writeFile(probe, body);
  const child = spawnSync(process.execPath, ["--import", "tsx", probe], {
    cwd: PACKAGE_ROOT,
    env: { [CONFIG_ENV_VAR]: configPath },
    encoding: "utf8",
  });
  assert.equal(child.status, 0, child.stderr);
  return child.stdout;
}

test("the repo-wide runner reads each app's own config, not ZEROSHIP_CONFIG", async () => {
  const fx = await makeWorkspace();
  try {
    const appAConfig = join(fx.root, APP_A.dir, "zeroship.jsonc");
    const stdout = await runWithRedirectedConfig(fx.root, appAConfig);
    assert.match(stdout, /regen-ok/, stdout);

    const envA = await fs.readFile(join(fx.root, APP_A.dir, APP_A.out, ENV_DB_FILE), "utf8");
    assert.match(envA, /notes_a/, "app A regenerated from its own migrations");
    assert.match(envA, /alpha: Db</, "app A declares its own database label");

    const envB = await fs.readFile(join(fx.root, APP_B.dir, APP_B.out, ENV_DB_FILE), "utf8");
    assert.match(envB, /notes_b/, "app B regenerated from its own migrations");
    assert.match(envB, /beta: Db</, "app B declares its own database label");
    assert.doesNotMatch(envB, /alpha/, "app B did not inherit app A's config");
  } finally {
    await fx.cleanup();
  }
});
