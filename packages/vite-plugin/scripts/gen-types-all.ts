/**
 * REPO-WIDE gen-types regeneration — the entry point for every committed
 * `generated/zeroship/` artifact in the tree.
 *
 * WHY REPO-LEVEL. The gen-types emitter has generator INPUTS that live in this
 * package (`src/gen-types/confined-ceiling.ts`, `src/gen-types/render-env-db.ts`,
 * the `zeroship-migrate-node` fold). Changing one of them stales EVERY committed
 * artifact in the repo at once — that is exactly what 17cd17b46 did when it
 * added `default = "1"` to the injected `version` column. Only two of those
 * artifact sets are gated by a test (`test/gen-types/generated-source.test.ts`),
 * so the rest drift silently. A per-app script would just wait for the next
 * input change; this walks them all.
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
 * SOURCE SELECTION mirrors the plugin: a migrations dir - named by the app's
 * `zeroship.jsonc`, or `migrations/` when it has none - means the GENERATED
 * source (`genTypesFromMigrations`), otherwise a committed `schema.ts` means the
 * MANUAL source (`genTypesFromSchemaFile`). Neither present is a hard error, not
 * a silent skip.
 *
 * IT READS THE CONFIG NOW, which is the point of `zeroship.jsonc`. Before, this
 * runner REFUSED to run against any app whose vite config mentioned
 * `migrations:` or `genTypesOut` - by regex over the config source text,
 * because there was no machine-readable place to look. See `runOne`.
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
import { fileURLToPath } from "node:url";

import {
  ENV_DB_FILE,
  RUNTIME_DESCRIPTOR_FILE,
  genTypesFromMigrations,
  genTypesFromSchemaFile,
} from "../src/gen-types/index.js";
import { readProjectConfig } from "../src/project-config/index.js";

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
  // The app's own `zeroship.jsonc` when it has one, schema defaults otherwise.
  //
  // THIS REPLACES A HARD REFUSAL. `assertNoConfigOverride` used to throw for any
  // app whose vite config mentioned `migrations:` or `genTypesOut`, detected by
  // regex over the config SOURCE TEXT, because there was no machine-readable
  // place to look. That refusal was the clearest single piece of evidence that
  // `zeroship.jsonc` needed to exist; with the file in place it has nothing
  // left to protect, and the regex (which would match a comment and miss a
  // spread) goes with it.
  const { config } = readProjectConfig(app.root);
  const migrationsDir = resolve(app.root, config.migrations.dir);
  if (existsSync(migrationsDir) && statSync(migrationsDir).isDirectory()) {
    await genTypesFromMigrations(migrationsDir, app.outDir, { check });
    return "migrations";
  }
  for (const rel of ["schema.ts", "src/schema.ts"]) {
    const schemaTs = join(app.root, rel);
    if (existsSync(schemaTs)) {
      await genTypesFromSchemaFile(schemaTs, app.outDir, { check });
      return `schema (${rel})`;
    }
  }
  throw new Error(
    `gen-types-all: ${app.root} has committed ${RUNTIME_DESCRIPTOR_FILE} + ${ENV_DB_FILE} ` +
      `but no schema source (no migrations/ dir, no schema.ts) — the artifacts cannot be regenerated`,
  );
}

async function main(): Promise<void> {
  const check = process.argv.slice(2).includes("--check");
  const root = repoRoot();

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

await main();
