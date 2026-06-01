"use server";

import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Invariant (docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md):
// the console is a PURE creator app — it imports `@zeroship/control`
// NOWHERE. This guard walks the whole builder `src/` tree and fails if
// any source file references the control SDK (import, re-export, or
// dynamic import). Would FAIL on the pre-change tree (control-client.ts,
// apps.ts, tools.ts, http.ts all referenced it).

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC_ROOT = join(HERE, ".."); // apps/zeroship-builder/src

const SOURCE_EXT = /\.(ts|tsx|js|jsx|mjs|cjs)$/;
// Match the package name only as a real module specifier, not in prose.
const CONTROL_REF = /["'`]@zeroship\/control(?:\/[^"'`]*)?["'`]/;

// Test files are excluded: the guard scans SHIPPED source (what the
// bundle imports), and a test may legitimately name the package in an
// assertion (this file's own regex does). The build is the bundle
// contract; tests don't ship.
function collectSourceFiles(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    if (entry === "node_modules" || entry === "dist") continue;
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      collectSourceFiles(full, out);
    } else if (SOURCE_EXT.test(entry) && !/\.test\.(ts|tsx|js|jsx|mjs|cjs)$/.test(entry)) {
      out.push(full);
    }
  }
  return out;
}

describe("console is a pure creator app", () => {
  it("imports @zeroship/control nowhere in src/", () => {
    const offenders: string[] = [];
    for (const file of collectSourceFiles(SRC_ROOT)) {
      const text = readFileSync(file, "utf8");
      if (CONTROL_REF.test(text)) {
        offenders.push(relative(SRC_ROOT, file));
      }
    }
    expect(offenders).toEqual([]);
  });
});
