import { cp, mkdir, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [root, work] = process.argv.slice(2);
const dist = join(root, "packages/vite-plugin/dist");
const { genTypesFromMigrations } = await import(pathToFileURL(join(dist, "gen-types/index.js")));
const { recordApplyRequest } = await import(pathToFileURL(join(dist, "gen-types/apply-request.js")));
await mkdir(work, { recursive: true });
await symlink(join(root, "packages/vite-plugin/node_modules"), join(work, "node_modules"), "dir");
await cp(new URL("./migrations/", import.meta.url), join(work, "migrations"), { recursive: true });
await genTypesFromMigrations(join(work, "migrations"), join(work, "generated"), { check: false });
// The apply body is NOT an artifact: `zeroship migrate` records it in memory and
// posts it. This fixture is a Rust test, so it lands the same recording in the
// test's own scratch dir for `fleet.rs` to read back.
await writeFile(
  join(work, "generated", "apply-request.json"),
  await recordApplyRequest(join(work, "migrations")),
);
