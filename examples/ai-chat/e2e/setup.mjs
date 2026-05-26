// One-shot setup for the e2e driver: symlink the Nix-provided
// `playwright` + `playwright-core` packages into local node_modules so
// Node's ESM resolver can find them transitively. We avoid adding
// playwright as a real dependency to keep the demo's installable
// surface small.
//
// Run automatically by `pnpm e2e` (see package.json).

import { existsSync, readdirSync, symlinkSync, mkdirSync } from "node:fs";
import { execSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const nm = resolve(root, "node_modules");

function findPlaywrightStoreRoot() {
  // 1. PATH probe — `nix develop` puts `playwright` in PATH. Walk
  //    PATH ourselves; some shells' `which` shim resolves symlinks
  //    inconsistently across nix-shell setups.
  for (const dir of (process.env.PATH ?? "").split(":")) {
    if (!dir) continue;
    const cli = `${dir}/playwright`;
    if (existsSync(cli)) {
      const storeRoot = dir.replace(/\/bin\/?$/, "");
      if (existsSync(`${storeRoot}/lib/node_modules/playwright`)) return storeRoot;
    }
  }

  // 2. Fallback: scan /nix/store for any playwright-test bundle.
  try {
    const candidates = readdirSync("/nix/store").filter((d) =>
      d.startsWith("playwright-test-"),
    );
    for (const d of candidates) {
      const p = `/nix/store/${d}/lib/node_modules/playwright`;
      if (existsSync(p)) return `/nix/store/${d}`;
    }
  } catch (_) { /* fall through */ }

  return null;
}

const storeRoot = findPlaywrightStoreRoot();
if (!storeRoot) {
  console.error(
    "[setup] Could not locate playwright-test in /nix/store. Run inside `nix develop`.",
  );
  process.exit(1);
}

if (!existsSync(nm)) mkdirSync(nm);

for (const pkg of ["playwright", "playwright-core"]) {
  const target = `${storeRoot}/lib/node_modules/${pkg}`;
  const link = `${nm}/${pkg}`;
  if (existsSync(link)) continue;
  if (!existsSync(target)) {
    console.error(`[setup] missing source: ${target}`);
    process.exit(1);
  }
  symlinkSync(target, link, "dir");
  console.log(`[setup] linked ${pkg} → ${target}`);
}
