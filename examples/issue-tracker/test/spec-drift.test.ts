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
