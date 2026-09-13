// Emit the remaining dispatcher as an importable side-effect script.

import { readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const distDir = resolve(__dirname, "../dist");

const SPLICED_FILES = [
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
