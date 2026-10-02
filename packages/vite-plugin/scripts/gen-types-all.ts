/**
 * REPO-WIDE gen-types regeneration — the entry point for every committed
 * `generated/zeroship/` artifact in the tree.
 *
 * WHY REPO-LEVEL. The gen-types emitter has generator INPUTS that live in this
 * package (`src/gen-types/confined-ceiling.ts`, `src/gen-types/render-env-db.ts`,
 * the `zeroship-migrate-node` fold). Changing one of them stales EVERY committed
 * artifact in the repo at once, as adding `default = "1"` to the injected
 * `version` column does. Only the artifact sets
 * `test/gen-types/generated-source.test.ts` gates are covered, so the rest
 * drift silently. A per-app script would just wait for the next input change;
 * this walks them all.
 *
 * WHY IT LIVES HERE. The artifacts are produced by the in-process `gen-types`
 * library (no CLI, no subprocess), so the regenerator must run from a package
 * that can import it — i.e. one with `node_modules`. `@zeroship/vite-plugin`
 * owns the library, so it hosts the runner. The scaffold template in particular
 * CANNOT host its own script: it is a subdirectory of
 * `packages/create-zeroship-app`, not a workspace package, it has no
 * `node_modules`, and it must not get one (it is copied verbatim into a new
 * creator app).
 *
 * ENUMERATION — never a hardcoded list. Walk the repo for any directory holding
 * a `schema.runtime.json` (the filename comes from the emitter's own
 * `RUNTIME_DESCRIPTOR_FILE` constant, so it tracks the emitter). Each hit is an
 * out dir; its app root is the nearest ancestor with a `package.json`. A new
 * example is therefore covered the day its artifacts appear, with no edit here.
 *
 * SOURCE SELECTION mirrors the plugin: the database whose `out` IS this artifact
 * directory, named by the app's `zeroship.jsonc`, supplies the migrations dir and
 * means the GENERATED source (`genTypesFromMigrations`); otherwise a committed
 * `schema.ts` means the MANUAL source (`genTypesFromSchemaFile`). Neither present
 * is a hard error, not a silent skip.
 *
 * EACH APP READS ITS OWN FILE. `runOne` reads the `zeroship.jsonc` at the app
 * root with an empty process environment, so a `ZEROSHIP_CONFIG` set in the
 * shell cannot redirect the whole walk at one app's file; see `runOne`.
 *
 *   pnpm --filter @zeroship/vite-plugin gen-types:all             # regenerate
 *   pnpm --filter @zeroship/vite-plugin gen-types:all -- --check  # drift gate
 *
 * `--check` never writes: it regenerates in memory and fails naming the drifted
 * file. That is the CI-shaped inverse of the default write.
 */

import { execFileSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { readdir } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  ENV_DB_FILE,
  RUNTIME_DESCRIPTOR_FILE,
  genTypesFromMigrations,
  genTypesFromSchemaFile,
} from "../src/gen-types/index.js";
import { databaseForOutDir, readProjectConfig } from "../src/project-config/index.js";

/** Directories a repo walk must never descend into: build output, dependency
 *  trees, sibling git checkouts, and the vendored engine. */
const SKIP_DIRS = new Set([
  ".git",
  ".worktrees",
  "node_modules",
  "dist",
  "target",
  ".zeroship",
  "third_party",
  ".turbo",
  ".vite",
]);

/** An app whose committed gen-types artifacts this runner owns. */
interface ArtifactApp {
  /** Nearest ancestor of `outDir` holding a `package.json`. */
  root: string;
  /** The directory holding `env.db.ts` + `schema.runtime.json`. */
  outDir: string;
}

/** The repo root, resolved from git so the runner works from any cwd. */
function repoRoot(): string {
  return execFileSync("git", ["rev-parse", "--show-toplevel"], {
    cwd: dirname(fileURLToPath(import.meta.url)),
    encoding: "utf8",
  }).trim();
}

/** App artifact pairs; standalone Rust descriptor fixtures have their own producer tests. */
async function findArtifactDirs(dir: string, out: string[] = []): Promise<string[]> {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  if ([RUNTIME_DESCRIPTOR_FILE, ENV_DB_FILE].every((name) =>
    entries.some((e) => e.isFile() && e.name === name))) out.push(dir);
  for (const e of entries) {
    if (!e.isDirectory() || SKIP_DIRS.has(e.name)) continue;
    await findArtifactDirs(join(dir, e.name), out);
  }
  return out;
}

/** Walk up from `outDir` to the nearest ancestor with a `package.json`. */
function appRootFor(outDir: string, root: string): string {
  for (let dir = outDir; dir.startsWith(root); dir = dirname(dir)) {
    if (existsSync(join(dir, "package.json"))) return dir;
    if (dir === root) break;
  }
  throw new Error(
    `gen-types-all: no package.json above ${outDir} — cannot tell which app owns these artifacts`,
  );
}

/** Regenerate (or `--check`) one app's artifacts through the in-process API. */
async function runOne(app: ArtifactApp, check: boolean): Promise<string> {
  // Read THIS app's own `zeroship.jsonc`. The runner is repo-wide and may be
  // invoked with `ZEROSHIP_CONFIG` set in the shell; that variable names one
  // config file for a single build, so passing it through would point every app
  // in the walk at that one file. An empty process environment keeps config
  // discovery local to `app.root`, so each app's database label, migrations dir
  // and out dir come from its own file.
  const { config } = readProjectConfig(app.root, { processEnv: {} });
  // The artifacts were discovered by directory, and a workspace has one
  // directory per DATABASE, so the sources are the ones of the database whose
  // `out` is this directory.
  const database = databaseForOutDir(config, app.root, app.outDir);
  // THE DECLARATION IS REQUIRED, not merely a schema source. `env.db.ts`
  // declares this database's entry on `EnvDatabases` under its LABEL, and
  // `Env.db` only when an app names it `primary` - two facts that live in
  // `zeroship.jsonc` and nowhere in the fold. An out dir no `databases.*.out`
  // claims cannot be regenerated without guessing both.
  if (database == null) {
    throw new Error(
      `gen-types-all: ${app.outDir} holds committed ${RUNTIME_DESCRIPTOR_FILE} + ` +
        `${ENV_DB_FILE} but no \`databases.*.out\` in ${app.root} names it, so the label ` +
        `its artifacts declare on \`EnvDatabases\` is unknown`,
    );
  }
  const { label, primary } = database;
  const migrationsDir = resolve(app.root, database.migrations);
  if (existsSync(migrationsDir) && statSync(migrationsDir).isDirectory()) {
    await genTypesFromMigrations(migrationsDir, app.outDir, { label, primary, check });
    return `migrations (${label})`;
  }
  for (const rel of ["schema.ts", "src/schema.ts"]) {
    const schemaTs = join(app.root, rel);
    if (existsSync(schemaTs)) {
      await genTypesFromSchemaFile(schemaTs, app.outDir, { label, primary, check });
      return `schema (${rel})`;
    }
  }
  throw new Error(
    `gen-types-all: ${app.root} has committed ${RUNTIME_DESCRIPTOR_FILE} + ${ENV_DB_FILE} ` +
      `but no schema source (no migrations/ dir, no schema.ts) — the artifacts cannot be regenerated`,
  );
}

/** Regenerate (or `--check`) every artifact set under `root`. */
export async function regenerateAll(root: string, check: boolean): Promise<void> {
  const outDirs = (await findArtifactDirs(root)).sort();
  if (outDirs.length === 0) {
    throw new Error(`gen-types-all: found no ${RUNTIME_DESCRIPTOR_FILE} anywhere under ${root}`);
  }
  const apps: ArtifactApp[] = outDirs.map((outDir) => ({ outDir, root: appRootFor(outDir, root) }));

  console.log(
    `[gen-types-all] ${check ? "checking" : "regenerating"} ${apps.length} artifact set(s) under ${root}`,
  );

  const failures: string[] = [];
  for (const app of apps) {
    const label = relative(root, app.outDir);
    try {
      const source = await runOne(app, check);
      console.log(`  ok   ${label}  [${source}]`);
    } catch (e) {
      console.error(`  FAIL ${label}\n       ${(e as Error).message}`);
      failures.push(label);
    }
  }

  if (failures.length > 0) {
    throw new Error(
      `gen-types-all: ${failures.length} of ${apps.length} artifact set(s) failed: ${failures.join(", ")}`,
    );
  }
  console.log(`[gen-types-all] ${check ? "no drift" : "done"} — ${apps.length} artifact set(s)`);
}

async function main(): Promise<void> {
  const check = process.argv.slice(2).includes("--check");
  await regenerateAll(repoRoot(), check);
}

if (process.argv[1] != null && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
