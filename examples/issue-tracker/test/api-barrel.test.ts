import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

/**
 * Every server procedure must be re-exported from `src/api.ts`.
 *
 * WHY THIS TEST EXISTS. A procedure that is not in the barrel is invisible to
 * the client: the Vite plugin rewrites imports from `./api` into HTTP-RPC
 * stubs, so a handler missing from that file simply cannot be called from the
 * browser. Nothing else catches it -- the server typechecks, the build reports
 * the procedure, the deploy includes it, and the smoke suite drives it over
 * HTTP and passes. Only the UI is affected, and only by silently having no way
 * to reach the feature.
 *
 * This happened three separate times in this example: flags.list and
 * cc.listMine were added and left unreachable, and then eleven access-control,
 * voting and watching procedures after them. Each was found by accident while
 * doing something else.
 *
 * The check is textual on purpose. Importing both modules would pull in the
 * `zeroship` runtime, which does not exist under vitest, and comparing what
 * the module system exports would test the wrong thing anyway: the failure is
 * that a NAME is missing from one file, not that a binding is broken.
 */

// Resolved from the package root (vitest runs with cwd there) rather than
// import.meta.url, which this config rewrites to an absolute-looking "/src/...".
const src = (name: string) => readFileSync(resolve(process.cwd(), "src", name), "utf8");

/** `export const listBugs = query(` -> `listBugs`. */
function serverProcedures(indexSource: string): string[] {
  const pattern = /^export const ([A-Za-z0-9_]+) = (?:query|mutation|action|stream)\(/gm;
  const names: string[] = [];
  for (const match of indexSource.matchAll(pattern)) names.push(match[1]);
  return names;
}

/** The identifiers listed inside `export { ... } from "./index"`. */
function barrelExports(apiSource: string): Set<string> {
  const block = apiSource.match(/export\s*\{([\s\S]*?)\}\s*from\s*"\.\/index";/);
  if (!block) throw new Error("src/api.ts has no `export { ... } from \"./index\"` block");
  // Comments are stripped BEFORE the split, not after. Splitting first breaks
  // a comment containing a comma ("// Dependencies, duplicates") into
  // fragments, and the tail of one trims to a bare word that looks exactly
  // like an exported identifier -- which reported a phantom stale export.
  return new Set(
    block[1]
      .replace(/\/\/.*$/gm, "")
      .split(",")
      .map((entry) => entry.trim())
      .filter((entry) => /^[A-Za-z0-9_]+$/.test(entry)),
  );
}

describe("the client RPC barrel", () => {
  const index = src("index.ts");
  const api = src("api.ts");

  it("finds the procedures it is meant to compare", () => {
    // Guards the regex, not the app. If a refactor changes how procedures are
    // declared, this test would otherwise pass by comparing an empty list to
    // an empty list -- green, and measuring nothing.
    expect(serverProcedures(index).length).toBeGreaterThan(50);
    expect(barrelExports(api).size).toBeGreaterThan(50);
  });

  it("re-exports every server procedure", () => {
    const exported = barrelExports(api);
    const missing = serverProcedures(index).filter((name) => !exported.has(name));
    expect(
      missing,
      `these procedures exist on the server but are not exported from src/api.ts, ` +
        `so no client code can call them: ${missing.join(", ")}`,
    ).toEqual([]);
  });

  it("does not export names the server no longer defines", () => {
    // The other direction: a renamed or deleted procedure left in the barrel
    // is a build error only once something imports it.
    const defined = new Set(serverProcedures(index));
    const stale = [...barrelExports(api)].filter((name) => !defined.has(name));
    expect(stale, `exported from src/api.ts but not defined in src/index.ts: ${stale.join(", ")}`)
      .toEqual([]);
  });
});
