import { readFileSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * Every class a spec selects on still exists in the app.
 *
 * A selector that matches nothing fails LOUDLY in a positive assertion -- the
 * click throws, the expect times out, someone fixes it. In a negative
 * assertion it passes forever and silently:
 *
 *     await expect(page.locator(".bug-detail-side")).toHaveCount(0);
 *
 * After the entity rename that class became `.issue-detail-side`, and the line
 * above would have gone on reporting success about a thing it could no longer
 * find. There are 44 `toHaveCount(0)` assertions in this suite; each one is a
 * candidate for exactly that.
 *
 * The rename made this concrete rather than theoretical. It also produced the
 * sibling case that motivated the id-prefix pins in
 * `issue-reference-by-key.spec.ts`: a regex on `bug_` could never match after
 * ids became `issu_`, so an assertion that no raw id leaked into the page
 * became an assertion about nothing.
 *
 * WHAT THIS DOES NOT CATCH. It proves a class NAME appears somewhere in the
 * app source, not that it is rendered on the page the spec visits, nor that it
 * is on the element the spec means. A class mentioned only in a comment counts
 * as found. It is a smell detector for dead selectors, not a proof of live
 * ones -- the suite passing is what shows the selectors match real elements.
 *
 * It also says nothing about role- or text-based locators, which are the
 * majority here and are checked by the suite itself: `getByRole("tab", ...)`
 * cannot go quietly dead in the same way, because Playwright resolves roles
 * against the live accessibility tree.
 */

const root = process.cwd();

function filesUnder(dir: string, pattern: RegExp): string[] {
  const out: string[] = [];
  const walk = (d: string) => {
    for (const entry of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, entry.name);
      if (entry.isDirectory()) walk(p);
      else if (pattern.test(entry.name)) out.push(p);
    }
  };
  walk(resolve(root, dir));
  return out;
}

/** Class names appearing inside `locator(...)` / `querySelector(...)` calls. */
function classesUsedBySpecs(): Set<string> {
  const classes = new Set<string>();
  for (const file of filesUnder("e2e", /\.ts$/)) {
    const source = readFileSync(file, "utf8");
    for (const call of source.matchAll(
      /(?:locator|querySelector|querySelectorAll)\(\s*[`"']([^`"']+)[`"']/g,
    )) {
      for (const cls of call[1].matchAll(/\.([a-zA-Z][\w-]*)/g)) classes.add(cls[1]);
    }
  }
  return classes;
}

/** Everything the app could render a class from. */
function appSource(): string {
  const sources = filesUnder("src", /\.(tsx?|css)$/);
  return sources.map((f) => readFileSync(f, "utf8")).join("\n");
}

describe("e2e selectors", () => {
  it("finds the selectors at all", () => {
    // Guard: if the extraction regex stops matching, the assertion below
    // passes against an empty set and this file quietly stops working.
    expect(
      classesUsedBySpecs().size,
      "no class selectors parsed out of e2e/; the extractor is broken, not the specs",
    ).toBeGreaterThan(30);
  });

  it("selects only classes the app can render", () => {
    const haystack = appSource();
    const dead = [...classesUsedBySpecs()].filter((cls) => !haystack.includes(cls)).sort();

    expect(
      dead,
      "these classes are selected by specs but appear nowhere in src/. A " +
        "positive assertion on them fails loudly; a negative one (toHaveCount(0), not.toBeVisible) " +
        "passes forever while testing nothing:\n  ." +
        dead.join("\n  ."),
    ).toEqual([]);
  });
});
