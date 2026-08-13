import { readdirSync, readFileSync } from "node:fs";
import { join, relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";

/**
 * No component is defined and rendered nowhere.
 *
 * This has happened twice, both times by removing the page that hosted
 * something rather than the something itself:
 *
 *  - `QuickSearchBox` survived the deletion of the advanced-search page, so
 *    `search.quick` became unreachable from the UI while the procedure, its
 *    policy entry and its parser all still existed.
 *  - `Nav` survived the move to AppShell's sidebar rail.
 *
 * Neither failed anything. An orphan compiles, typechecks, and is invisible to
 * every browser spec because there is nothing to drive. The only signal is
 * that no JSX anywhere mentions it.
 *
 * A unit test rather than a browser one: this is a fact about the source, and
 * asking a running page about a component that is not on it proves nothing.
 */

const SRC = resolve(process.cwd(), "src");

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...sourceFiles(path));
    else if (/\.tsx?$/.test(entry.name)) out.push(path);
  }
  return out;
}

describe("component reachability", () => {
  it("every component is rendered somewhere", () => {
    const files = sourceFiles(SRC);
    const all = files.map((file) => readFileSync(file, "utf8")).join("\n");

    const orphans: string[] = [];
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      // A component is a capitalised function declaration. Hooks (useX) and
      // helpers (lowercase) are excluded by the capital, and a component used
      // only through a compound parent (Dialog.Body) is matched by the parent.
      for (const match of text.matchAll(/^(?:export )?function ([A-Z][A-Za-z0-9]*)\(/gm)) {
        const name = match[1];
        if (new RegExp("<" + name + "[\\s/>]").test(all)) continue;
        orphans.push(`${relative(SRC, file)}: ${name}`);
      }
    }

    expect(
      orphans,
      `components defined but never rendered -- delete them, or render them:\n${orphans.join("\n")}`,
    ).toEqual([]);
  });
});
