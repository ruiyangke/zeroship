import { cp, mkdir, symlink } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [root, work] = process.argv.slice(2);
const { genTypesFromMigrations } = await import(pathToFileURL(join(root, "sdks/vite-plugin/dist/gen-types/index.js")));
await mkdir(work, { recursive: true });
await symlink(join(root, "sdks/vite-plugin/node_modules"), join(work, "node_modules"), "dir");
await cp(new URL("./migrations/", import.meta.url), join(work, "migrations"), { recursive: true });
await genTypesFromMigrations(join(work, "migrations"), join(work, "generated"), { check: false });
