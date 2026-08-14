// The tutorial's first minute: `new` then `lint`.
//
// `docs/getting-started.md` opens with `zero-migrate new create_users` followed
// immediately by `lint --dir ./migrations --explain`, before any database is
// involved. That is the first thing a new user runs, and if the scaffold the
// generator writes did not lint, the tutorial would fail at step one.
//
// Nothing covered it. `cli.test.ts` exercises `new` (that it writes a file, and
// its naming rules) and `lint` separately; no test feeds one into the other. The
// scaffold is a template string in the generator, so a DSL rename, a stray
// syntax error, or a change to the default export shape would leave every
// existing test green and every new user stuck.
//
// The assertions are deliberately about the CONTRACT rather than the wording:
// the file must parse, export a `name` and a default object with a `schema`, and
// lint must accept it on every dialect. A test that pinned the comment text would
// fail on any harmless copy edit while still missing a broken template.
//
// ZERO OPS IS THE POINT, not a weakness. The scaffold is meant to be an empty
// starting point with the DSL shown in comments, so `lint` reporting `ok (0 ops)`
// is the correct answer - and asserting it pins that the generator does not emit
// a stub operation someone would have to remember to delete.
//
// GATE: none. `new` and `lint` are both offline.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

function run(argv: string[]): { code: number | null; out: string; err: string } {
  const result = spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...argv], {
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  return {
    code: result.status,
    out: result.stdout ?? "",
    err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("a freshly generated migration lints clean on every dialect", () => {
  const dir = mkdtempSync(join(HERE, "scaffold-"));
  try {
    const created = run(["new", "create_users", "--dir", dir]);
    assert.equal(created.code, 0, `new must succeed; ${created.err}`);

    const files = readdirSync(dir).filter((name) => name.endsWith(".ts"));
    assert.equal(files.length, 1, `new must write exactly one migration; got ${files.join(",")}`);
    assert.match(
      files[0],
      /^\d{14}_create_users\.ts$/,
      `the file must carry a timestamp and the authored name; got ${files[0]}`,
    );

    // The contract the engine relies on, checked without pinning prose: any
    // harmless edit to the guidance comments must not fail this test, and a
    // broken template must not pass it.
    const source = readFileSync(join(dir, files[0]), "utf8");
    assert.match(source, /export const name = "create_users"/, "it must export the migration name");
    assert.match(source, /export default \{/, "it must have a default export");
    assert.match(source, /\bschema\(\)/, "whose object provides schema()");

    // The tutorial's next command, verbatim.
    const linted = run(["lint", "--dir", dir, "--explain"]);
    assert.equal(linted.code, 0, `the scaffold must lint clean; ${linted.err || linted.out}`);

    // Zero ops is correct: the scaffold is an empty starting point with the DSL
    // in comments. Asserting it pins that no stub operation is emitted for the
    // user to discover and delete.
    assert.match(
      linted.out,
      /lint create_users: ok \(0 ops\)/,
      `lint must report the scaffold as ok with no operations; got ${linted.out.slice(0, 200)}`,
    );

    // `lint` with no --dialect checks all three, so this covers the scaffold
    // being acceptable everywhere rather than only on the default target.
    for (const dialect of ["postgres", "mysql", "sqlite"]) {
      assert.match(
        linted.out,
        new RegExp(`dialect: ${dialect}`),
        `the default lint must cover ${dialect}`,
      );
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
