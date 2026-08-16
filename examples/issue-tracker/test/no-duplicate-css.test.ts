import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * A selector is written once.
 *
 * `src/styles.css` had six selectors defined twice, hundreds of lines apart,
 * AFTER an earlier pass had already cleaned duplicates out of it. That is the
 * third convention this codebase has lost by relying on someone to re-audit
 * it, alongside the RPC auth policy that quietly dropped `groups.delete` and
 * the cache rule that five components had drifted out of. So this is a test
 * rather than a habit.
 *
 * THE DAMAGE HAS TWO MODES, and the harmless one is why the pattern survives:
 *
 *   Same property in both blocks -- the later silently wins and the earlier is
 *   dead code that misleads whoever reads it first. That is the `.tabs` block
 *   whose active-tab colour was overridden 457 lines away, and the
 *   `.app-textarea:focus-visible` whose halo was overridden by a second copy,
 *   which cost a whole debugging session chasing a focus ring that was being
 *   drawn and then undrawn.
 *
 *   Disjoint properties -- nothing looks wrong at all, both blocks are live,
 *   and the rule simply cannot be understood by reading either one. Four of
 *   the six were this kind. Nothing would ever have surfaced them.
 *
 * TWO THINGS THIS PARSER HAS TO GET RIGHT, both learned by getting them wrong.
 *
 *   It compares WHOLE selector lists. A first version split on commas and
 *   counted each part, which flagged `h1, h2, h3, h4 { margin }` plus
 *   `h1 { font-size }` as a duplicate `h1`. That is ordinary CSS authoring; it
 *   reported 17 duplicates where there were 6, and a check that cries wolf on
 *   normal code is worse than no check because it gets ignored.
 *
 *   It skips at-rules. The same selector inside two different `@media` blocks
 *   is legitimate responsive authoring, and a selector inside an at-rule is a
 *   different rule from the one outside it.
 *
 * WHAT THIS DOES NOT CATCH: duplicated DECLARATIONS across DIFFERENT selectors
 * -- two rules setting the same border on differently-named things -- which is
 * real duplication of a kind this says nothing about. It also cannot see a
 * selector that is effectively dead because nothing in the app carries that
 * class; `no-orphans.test.ts` is the neighbour for that.
 */

const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");

/** Selector lists of every rule at the TOP level, in source order. */
function topLevelSelectors(): string[] {
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const found: string[] = [];
  let depth = 0;
  let buffer = "";
  for (const ch of stripped) {
    if (ch === "{") {
      if (depth === 0) {
        const selector = buffer.trim().replace(/\s+/g, " ");
        // `@media`, `@supports`, `@keyframes`: their contents are a separate
        // cascade level, and the at-rule head is not a selector.
        if (selector && !selector.startsWith("@")) found.push(selector);
      }
      depth += 1;
      buffer = "";
    } else if (ch === "}") {
      depth -= 1;
      buffer = "";
    } else if (depth === 0) {
      buffer += ch;
    }
  }
  return found;
}

describe("the app stylesheet", () => {
  it("parses into a plausible number of rules", () => {
    // Guard. A regex that stops matching would make the assertion below pass
    // against an empty list, and this file would become decoration -- which is
    // the exact failure mode it exists to prevent elsewhere.
    expect(
      topLevelSelectors().length,
      "no top-level rules parsed out of src/styles.css; the parser is broken, not the stylesheet",
    ).toBeGreaterThan(0);
  });

  it("defines every selector exactly once", () => {
    const counts = new Map<string, number>();
    for (const selector of topLevelSelectors()) {
      counts.set(selector, (counts.get(selector) ?? 0) + 1);
    }
    const duplicated = [...counts.entries()]
      .filter(([, n]) => n > 1)
      .map(([selector, n]) => `${selector} (${n}x)`);

    expect(
      duplicated,
      "these selectors are defined more than once in src/styles.css. Merge them into a single " +
        "rule at the LATER location, computing each property's winning value by source order, " +
        "and note on the survivor that it absorbed an earlier definition:\n  " +
        duplicated.join("\n  "),
    ).toEqual([]);
  });
});
