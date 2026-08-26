// Every markdown link between shipped documents points at a file that exists.
//
// `docs-name-real-cli.test.ts` checks that the docs name real FLAGS. Nothing
// checked that they name real FILES. A link is the other half of the same
// promise: `[Policy model](policy.md)` is a claim about the repository, and a
// renamed or deleted page breaks it silently — markdown has no compiler, and a
// dead link reads exactly like a live one until someone clicks it.
//
// This is the third shipped surface in a row that nothing verified (F530: the
// example project; F531: the READMEs), and it is checked here for the same
// reason: these files are read far more often than they are exercised.
//
// CONTRIBUTING.md is included, unlike in the flag verifier. There the cost was
// eight `FOREIGN_FLAGS` entries for another tool's build flags; a link either
// resolves or does not, so there is no allowlist to dilute.
//
// Anchors are deliberately NOT checked. `page.md#section` requires parsing
// headings and mirroring GitHub's slug rules, and a wrong slug rule would produce
// confident false failures — worse than the gap it closes. The file half is
// checked; the anchor half is left honestly unchecked rather than checked badly.
//
// External `http(s)` links are not checked either: that would put the suite on the
// network and make it fail for reasons that have nothing to do with this repo.
//
// No GATE: pure filesystem, no database.

import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "../../../..");
const DOCS = join(ROOT, "docs");

/** Every shipped document that carries links a reader may follow. */
function documents(): string[] {
  const top = [
    join(ROOT, "README.md"),
    join(ROOT, "CONTRIBUTING.md"),
    join(ROOT, "packages", "zero-migrate-cli", "README.md"),
    join(ROOT, "packages", "zero-migrate", "README.md"),
  ].filter((path) => existsSync(path));
  const docs = readdirSync(DOCS)
    .filter((name) => name.endsWith(".md") && name !== "review-log.md")
    .map((name) => join(DOCS, name));
  return [...top, ...docs];
}

test("every markdown link in the shipped documents resolves to a real file", () => {
  const files = documents();
  assert.ok(files.length > 10, `the scan must find documents to check; found ${files.length}`);

  const broken: string[] = [];
  let checked = 0;
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    // `](target.md)` and `](target.md#anchor)`; the anchor is dropped.
    for (const match of text.matchAll(/\]\(([^)#\s]+\.md)(?:#[^)]*)?\)/g)) {
      const target = match[1];
      if (/^https?:/.test(target)) continue;
      checked += 1;
      if (!existsSync(resolve(dirname(file), target))) {
        broken.push(`${file.replace(`${ROOT}/`, "")} -> ${target}`);
      }
    }
  }

  // Non-vacuity: a regex that matched nothing would report zero broken links and
  // mean nothing, which is the failure mode of every "no problems found" check.
  assert.ok(checked > 50, `the scan must find links to check; found ${checked}`);
  assert.deepEqual(broken, [], "a shipped document links to a file that does not exist");
});
