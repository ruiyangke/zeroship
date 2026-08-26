// Every `allowBuilds` entry in `pnpm-workspace.yaml` must be an actual boolean.
//
// `allowBuilds` decides whether a dependency's `preinstall`/`postinstall` scripts
// run. pnpm stopped running them by default in v10 precisely because they are
// arbitrary code from the dependency tree, so each entry is a security decision
// someone made on purpose.
//
// pnpm does NOT validate the value. A committed placeholder
// (`esbuild: set this to true or false`) was accepted without a word and written
// verbatim into `node_modules/.modules.yaml` as though it were an answer -- so the
// tree carried an unanswered question in the position where a decision belongs,
// and no gate anywhere could see it. That is what this test exists for: pnpm will
// not tell you, and a green install is not evidence.
//
// Checks the SHAPE, not the answer. `esbuild: false` is a legitimate decision this
// gate must accept; the failure it catches is a value that is neither decision. It
// also rejects the string spellings `"true"` and `"false"`, which read as answers
// and are not booleans to a YAML parser.
//
// Deliberately hand-parsed rather than pulled through a YAML library: the file is a
// build input this repo has no other reason to depend on a parser for, and the two
// forms `allowBuilds` accepts (a `key: value` map, optionally with quoted keys like
// `nx@21.6.4`) are small enough to read directly. A parser would also normalise
// away the exact spelling this gate is about.
//
// Does NOT check that the packages named exist, does NOT check `onlyBuiltDependencies`
// (superseded by `allowBuilds` in pnpm 10.26 and removed in 11.0), and does NOT
// verify that an allowed build actually ran -- that is the install's business.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const workspaceFile = join(resolve(here, "../../.."), "pnpm-workspace.yaml");

/** The `package: value` pairs under the top-level `allowBuilds:` key, as written.
 *  Values are returned as RAW TEXT so the caller can judge the spelling; parsing
 *  them into booleans here would discard the very thing being checked. */
function allowBuildsEntries(yaml: string): Array<{ pkg: string; raw: string; line: number }> {
  const lines = yaml.split("\n");
  const start = lines.findIndex((line) => /^allowBuilds:\s*(#.*)?$/.test(line));
  if (start === -1) return [];

  const entries: Array<{ pkg: string; raw: string; line: number }> = [];
  for (let i = start + 1; i < lines.length; i += 1) {
    const line = lines[i]!;
    if (line.trim() === "" || line.trimStart().startsWith("#")) continue;
    // A non-indented line ends the block: the next top-level key.
    if (!/^\s/.test(line)) break;
    const match = /^\s+("[^"]+"|'[^']+'|[^:#\s][^:#]*?)\s*:\s*(.*?)\s*(?:#.*)?$/.exec(line);
    assert.ok(
      match,
      `pnpm-workspace.yaml:${i + 1} sits under allowBuilds: but is not a \`package: value\` ` +
        `pair this gate can read: ${JSON.stringify(line)}. Rather than skip it -- which would ` +
        "let an unreadable entry pass as checked -- teach this parser the form or simplify the file.",
    );
    entries.push({
      pkg: match[1]!.replace(/^["']|["']$/g, ""),
      raw: match[2]!,
      line: i + 1,
    });
  }
  return entries;
}

test("every pnpm allowBuilds entry is a real boolean", () => {
  const yaml = readFileSync(workspaceFile, "utf8");
  const entries = allowBuildsEntries(yaml);

  // A file with no `allowBuilds:` block is a legitimate state -- but so is a gate
  // that silently found nothing because the block was renamed or this parser broke.
  // Assert on the block's PRESENCE separately from its contents, so the two cases
  // cannot report the same green.
  assert.ok(
    /^allowBuilds:\s*(#.*)?$/m.test(yaml) === (entries.length > 0),
    "pnpm-workspace.yaml declares an `allowBuilds:` block but this gate read no entries " +
      "from it (or read entries without finding the block). Either way the parser above " +
      "no longer matches the file, and a scan that reads nothing must not report clean.",
  );

  const offenders = entries.filter((entry) => entry.raw !== "true" && entry.raw !== "false");
  assert.deepEqual(
    offenders.map((entry) => `pnpm-workspace.yaml:${entry.line} ${entry.pkg}: ${entry.raw}`),
    [],
    "an allowBuilds entry is not a boolean. The value decides whether that package's " +
      "install scripts execute, and pnpm accepts anything you write without complaint -- " +
      "including an instruction to yourself. Write `true` or `false`, unquoted.",
  );
});
