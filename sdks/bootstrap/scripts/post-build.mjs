// Post-build step for `@zeroship/bootstrap`.
//
// The runtime crate (`crates/runtime/src/core/init.rs`) `include_str!`s
// `dist/runtime-entry.js` and `dist/dispatcher.js` and SPLICES the
// content into the bootstrap ES module between `import * as user from
// "./__user__.js";` and the rest of the bootstrap body. The spliced
// content must therefore be:
//   - Pure top-level statements (no module-marker `export {}`).
//   - No source-map URL comments (would resolve relative to the
//     bootstrap's compiled name, not the original .ts file).
//
// tsc emits `export {};` for files that have no top-level imports or
// exports (so TS treats them as modules). We strip that line — the
// file is "module-like" from TS's POV but the emitted JS is consumed
// as inline script by the runtime crate.
//
// This script also rewrites the `await import("./install-schema.js")`
// inside `runtime-entry.js` to `await import("@zeroship/bootstrap/install-schema")`
// so the bundle resolver picks up the package's exports map at
// install time. (Relative imports work for a tsc-emitted ES module but
// the bootstrap module the runtime synthesizes uses bare-specifier
// resolution against the bundle's import map; bootstrap-package
// subpath imports go through the same path.)

import { readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const distDir = resolve(__dirname, "../dist");

const SPLICED_FILES = [
  "runtime-entry.js",
  "dispatcher.js",
];

for (const name of SPLICED_FILES) {
  const path = resolve(distDir, name);
  let src = await readFile(path, "utf8");
  // Strip `export {};` (with or without trailing semicolon / spaces).
  src = src.replace(/^export\s*\{\s*\}\s*;?\s*$/gm, "");
  // Strip source map comments — the spliced content is one chunk of
  // many in the bootstrap module; per-file maps would be meaningless.
  src = src.replace(/^\/\/# sourceMappingURL=.*$/gm, "");
  // Trim trailing blank lines so the splice point is tight.
  src = src.replace(/\n{3,}/g, "\n\n").trimEnd() + "\n";
  await writeFile(path, src, "utf8");
  console.log(`[post-build] cleaned ${name}`);
}
