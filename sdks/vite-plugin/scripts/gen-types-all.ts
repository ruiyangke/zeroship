/**
 * REPO-WIDE gen-types regeneration — the entry point for every committed
 * `generated/zeroship/` artifact in the tree.
 *
 * WHY REPO-LEVEL. The gen-types emitter has generator INPUTS that live in this
 * package (`src/gen-types/confined-ceiling.ts`, `src/gen-types/render-env-db.ts`,
 * the `zero-migrate-node` fold). Changing one of them stales EVERY committed
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
 * `sdks/create-zeroship-app`, not a workspace package, it has no
 * `node_modules`, and it must not get one (it is copied verbatim into a new
 * creator app).
 *
 * ENUMERATION — never a hardcoded list. Walk the repo for any directory holding
 * a `schema.runtime.json` (the filename comes from the emitter's own
 * `RUNTIME_DESCRIPTOR_FILE` constant, so it tracks the emitter). Each hit is an
 * out dir; its app root is the nearest ancestor with a `package.json`. A new
 * example is therefore covered the day its artifacts appear, with no edit here.
 *
 * SOURCE SELECTION mirrors the plugin: a `migrations/` dir means the GENERATED
 * source (`genTypesFromMigrations`), otherwise a committed `schema.ts` means the
 * MANUAL source (`genTypesFromSchemaFile`). Neither present is a hard error, not
 * a silent skip.
 *
 *   pnpm --filter @zeroship/vite-plugin gen-types:all             # regenerate
 *   pnpm --filter @zeroship/vite-plugin gen-types:all -- --check  # drift gate
 *
 * `--check` never writes: it regenerates in memory and fails naming the drifted
 * file. That is the CI-shaped inverse of the default write.
 */

import { execFileSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  ENV_DB_FILE,
  RUNTIME_DESCRIPTOR_FILE,
  genTypesFromMigrations,
  genTypesFromSchemaFile,
} from "../src/gen-types/index.js";

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

/** Every directory under `dir` that holds a `schema.runtime.json`. */
async function findArtifactDirs(dir: string, out: string[] = []): Promise<string[]> {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  if (entries.some((e) => e.isFile() && e.name === RUNTIME_DESCRIPTOR_FILE)) out.push(dir);
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

/**
 * Refuse to guess when an app overrides the gen-types input paths in its vite
 * config. This runner derives the migrations dir from disk; a `migrations: {...}`
 * option (a custom `dir` or `genTypesOut`) would silently regenerate from the
 * wrong source, which is worse than failing.
 */
async function assertNoConfigOverride(appRoot: string): Promise<void> {
  for (const name of ["vite.config.ts", "vite.config.js", "vite.config.mts"]) {
    const cfg = join(appRoot, name);
    if (!existsSync(cfg)) continue;
    const text = await readFile(cfg, "utf8");
    if (/\bgenTypesOut\b/.test(text) || /\bmigrations\s*:\s*\{/.test(text)) {
      throw new Error(
        `gen-types-all: ${relative(appRoot, cfg)} configures the gen-types inputs ` +
          `(migrations.dir / migrations.genTypesOut). This runner derives them from disk ` +
          `and will not guess — teach it to read the config, or drop the override.`,
      );
    }
  }
}

/** Regenerate (or `--check`) one app's artifacts through the in-process API. */
async function runOne(app: ArtifactApp, check: boolean): Promise<string> {
  await assertNoConfigOverride(app.root);

  const migrationsDir = join(app.root, "migrations");
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
