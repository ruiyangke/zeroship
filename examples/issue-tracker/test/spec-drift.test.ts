import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

/**
 * SPEC.md's "RPC surface" must list exactly the procedures the server defines.
 *
 * WHY. SPEC.md is the contract a reader trusts, and it drifted twice: it
 * documented `votes` and `watchers` as models while no code touched them, and
 * it went ~20 procedures out of date as access control, voting, watching and
 * see-also landed. Both times the gap was found by manually diffing, which is
 * exactly the kind of check that stops happening.
 *
 * Documentation that overstates is worse than none: a reader plans around a
 * feature that is not there. Understating is milder but still wrong -- it hides
 * the security surface, which is the part most worth reading.
 */

const read = (name: string) => readFileSync(resolve(process.cwd(), name), "utf8");

function implementedIds(indexSource: string): Set<string> {
  const ids = new Set<string>();
  for (const match of indexSource.matchAll(/\bid:\s*"([a-zA-Z]+\.[a-zA-Z]+)"/g)) {
    ids.add(match[1]);
  }
  return ids;
}

/**
 * Backticked dotted names inside the "## RPC surface" section only.
 *
 * Scoped to that section deliberately: the Models and Divergences prose also
 * mention procedures, and counting those would make the test pass for the
 * wrong reason -- a procedure named anywhere in the file would look documented
 * even if the surface list never mentioned it.
 */
function documentedIds(specSource: string): Set<string> {
  const start = specSource.indexOf("## RPC surface");
  const end = specSource.indexOf("## Frontend pages");
  if (start < 0 || end < 0 || end <= start) {
    throw new Error("SPEC.md is missing the '## RPC surface' or '## Frontend pages' heading");
  }
  const section = specSource.slice(start, end);
  const ids = new Set<string>();
  for (const match of section.matchAll(/`([a-zA-Z]+\.[a-zA-Z]+)`/g)) ids.add(match[1]);
  return ids;
}

describe("SPEC.md against the implementation", () => {
  const index = read("src/index.ts");
  const spec = read("SPEC.md");

  it("finds both surfaces it is meant to compare", () => {
    // Without this, a broken regex compares two empty sets and passes.
    expect(implementedIds(index).size).toBeGreaterThan(70);
    expect(documentedIds(spec).size).toBeGreaterThan(70);
  });

  it("documents every implemented procedure", () => {
    const documented = documentedIds(spec);
    const undocumented = [...implementedIds(index)].filter((id) => !documented.has(id)).sort();
    expect(
      undocumented,
      `implemented but absent from SPEC.md's RPC surface: ${undocumented.join(", ")}`,
    ).toEqual([]);
  });

  it("does not document procedures that do not exist", () => {
    const implemented = implementedIds(index);
    const phantom = [...documentedIds(spec)].filter((id) => !implemented.has(id)).sort();
    expect(
      phantom,
      `SPEC.md promises procedures the server does not define: ${phantom.join(", ")}`,
    ).toEqual([]);
  });
});

describe("README.md against the implementation", () => {
  const index = readFileSync(resolve(process.cwd(), "src/index.ts"), "utf8");
  const readme = readFileSync(resolve(process.cwd(), "README.md"), "utf8");
  const config = readFileSync(resolve(process.cwd(), "src/server/config.ts"), "utf8");

  /**
   * Only the two counts that MISLEAD when stale are gated, not every number in
   * the file. "82 procedures" and "nine are anonymous" are what a reader uses
   * to judge the size and the exposure of the app; a wrong anonymous count in
   * particular understates the public surface.
   */
  it("states the real procedure count", () => {
    const claimed = readme.match(/\*\*(\d+) explicitly named RPC procedures\*\*/)?.[1];
    expect(claimed, "README.md no longer states a procedure count in the expected form").toBeDefined();
    const actual = index.match(
      /^export const [A-Za-z0-9_]+ = (?:query|mutation|action|stream)\(/gm,
    )?.length;
    expect(Number(claimed)).toBe(actual);
  });

  it("states the real anonymous count", () => {
    const claimed = readme.match(/Nine are anonymous/i) ? 9 : null;
    expect(claimed, "README.md no longer states the anonymous count as a word").not.toBeNull();
    const actual = (config.match(/"rpc:[^"]+":\s*\{[^}]*auth:\s*"anon"/g) ?? []).length;
    expect(claimed).toBe(actual);
  });
});

describe("README.md's commands", () => {
  const readme = readFileSync(resolve(process.cwd(), "README.md"), "utf8");
  const scripts = Object.keys(
    JSON.parse(readFileSync(resolve(process.cwd(), "package.json"), "utf8")).scripts ?? {},
  );

  it("only tells the reader to run scripts that exist", () => {
    // A README naming a script that was renamed or never existed sends the
    // reader to `ERR_PNPM_NO_SCRIPT` on their first command. The digit in
    // `test:e2e` matters here -- an earlier version of this pattern stopped at
    // `[a-z:-]+` and silently truncated it to `test:e`, which would have made
    // this test pass while checking the wrong name.
    const referenced = [...readme.matchAll(/^pnpm ([a-z][a-z0-9:-]*)/gm)]
      .map((match) => match[1])
      .filter((name) => name !== "install");
    expect(referenced.length, "README no longer shows any pnpm commands").toBeGreaterThan(3);
    const missing = [...new Set(referenced)].filter((name) => !scripts.includes(name));
    expect(missing, `README runs scripts that package.json does not define: ${missing.join(", ")}`)
      .toEqual([]);
  });
});
