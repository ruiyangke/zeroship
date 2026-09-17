// One-shot setup for the e2e driver: make the `playwright` package
// resolvable from this example. If it already resolves there is nothing
// to do; otherwise the CLI on PATH is located and the package it owns is
// symlinked into node_modules. We avoid adding playwright as a real
// dependency to keep the demo's installable surface small.
//
// Run automatically by `pnpm e2e` (see package.json).

import { existsSync, symlinkSync, mkdirSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const nm = resolve(root, "node_modules");

function resolves(specifier) {
  try {
    createRequire(import.meta.url).resolve(specifier);
    return true;
  } catch {
    return false;
  }
}

// Walk PATH for the `playwright` CLI and return the prefix that holds it.
// The CLI sits in `bin/` beside `lib/node_modules/`, so the prefix is
// found without naming any install root. Done by hand rather than
// shelling out because a shell's `which` shim resolves symlinks
// inconsistently.
function findPlaywrightPrefix() {
  for (const dir of (process.env.PATH ?? "").split(":")) {
    if (!dir) continue;
    const cli = `${dir}/playwright`;
    if (!existsSync(cli)) continue;
    const prefix = dir.replace(/\/bin\/?$/, "");
    if (existsSync(`${prefix}/lib/node_modules/playwright`)) return prefix;
  }
  return null;
}

// Already resolvable means there is nothing to link: the ordinary state
// when playwright is a real dependency.
if (resolves("playwright")) {
  process.exit(0);
}

const prefix = findPlaywrightPrefix();
if (!prefix) {
  console.error(
    "[setup] Could not find the `playwright` package: it is not resolvable " +
      "from here and no `playwright` CLI is on PATH. " +
      "Install it (`pnpm add -D playwright`) or enter `nix develop`.",
  );
  process.exit(1);
}

if (!existsSync(nm)) mkdirSync(nm);

for (const pkg of ["playwright", "playwright-core"]) {
  const target = `${prefix}/lib/node_modules/${pkg}`;
  const link = `${nm}/${pkg}`;
  if (existsSync(link)) continue;
  if (!existsSync(target)) {
    console.error(`[setup] missing source: ${target}`);
    process.exit(1);
  }
  symlinkSync(target, link, "dir");
  console.log(`[setup] linked ${pkg} → ${target}`);
}
