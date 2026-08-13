// Everything the documentation tells an operator to type must exist.
//
// This session found four separate places where it did not:
//
//   the pending-contract refusal sent operators to `migrate resolve-pending
//   --apply`, a verb this project REMOVED;
//   `docs/cli.md` listed `squash` as a CLI writer, and `zero-migrate squash` is
//   an unknown command;
//   `docs/operations.md` said "The CLI has no history command", and it does;
//   `docs/writing-migrations.md` told authors to read a `--sql` preview, and no
//   verb accepts `--sql`.
//
// Four instances is a recurring class rather than four accidents, and each was
// found by hand after it shipped. This is the mechanical version, so the next one
// fails a test instead.
//
// IT CATCHES ONE OF THE FOUR, and only flags. That is stated here rather than
// implied, because a guard named after a class it half-covers is worse than a
// narrow one: it invites the assumption that the class is handled.
//
// A verb scan was written and removed. Prose about a CLI is English - "zero-
// migrate reads the journal", "zero-migrate never edits history" - and the CLI
// answers `unknown command reads` to all of it, so scanning prose measures
// grammar. Narrowing to fenced blocks fixed the noise and left five candidates,
// four of them still prose inside unlabelled fences. Meanwhile the three verb
// defects were themselves in prose ("apply and squash still wait for the lock",
// "the CLI has no history command"), so no version of that scan would have
// caught them. It was cut rather than kept as a passing test that checks little.
//
// Flags are different: `--sql` is unambiguous wherever it appears, so the scan
// reads whole documents, prose included, and would have caught it.
//
// Scanning flags across whole documents admits flags belonging to OTHER tools -
// the docs legitimately say `pnpm install --frozen-lockfile` - so those carry an
// explicit allowlist. The allowlist is the point: it is short, every entry names
// its owning tool, and anything not on it must be a real zero-migrate flag.
//
// This checks EXISTENCE, not correctness. A doc that names a real flag and
// describes it wrongly passes here; that is what reading is for.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const REPO_ROOT = resolve(HERE, "../../../..");
const DOCS = resolve(HERE, "../../../../docs");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

/**
 * Flags the docs name that belong to another tool. Each entry says whose it is;
 * an entry without an owner is a bug in this list rather than an exemption.
 */
const FOREIGN_FLAGS: Readonly<Record<string, string>> = {
  "--frozen-lockfile": "pnpm install",
  "--exact": "cargo test",
  "--lib": "cargo test",
  "--import": "node --import=tsx",
  "--flag": "cli.md's own placeholder in 'Value flags accept --flag value'",
};

/** The READMEs carry CLI instructions too, and are read FIRST.
 *
 * This verifier scanned `docs/` alone, which is the smaller half of the surface.
 * `README.md` is the first file anyone opens, and the three defects this file was
 * built to catch — a `squash` verb that does not exist, "the CLI has no history
 * command" when it has one, a `--sql` preview that was never a flag — are exactly
 * the kind that rot in a README while `docs/` stays current, because a README is
 * edited by people shipping features rather than people writing documentation.
 *
 * CONTRIBUTING.md is deliberately NOT included. It names eight cargo and pnpm
 * flags (`--workspace`, `--all-targets`, `--no-default-features` …) and no
 * zero-migrate ones, so adding it would mean eight `FOREIGN_FLAGS` entries for
 * another tool's build invocations — noise that dilutes the allowlist without
 * checking a single claim about this CLI.
 *
 * `crates/zero-migrate-node/README.md` was missing from this list until it was
 * checked directly. It carries no flags today, so nothing was wrong — but it is a
 * shipped README, and its absence was an accident of which paths someone happened
 * to type, not a decision like the CONTRIBUTING one above. */
const EXTRA_DOCS = [
  resolve(HERE, "../../../../README.md"),
  resolve(HERE, "../../README.md"),
  resolve(HERE, "../../../zero-migrate/README.md"),
  resolve(HERE, "../../../../crates/zero-migrate-node/README.md"),
];

/**
 * `review-log.md` is a historical record, not instructions to follow.
 *
 * `docs/proposals/` is EXCLUDED ON PURPOSE, and the exclusion must stay. This
 * scan asserts that every flag a document names exists in the real CLI; a
 * proposal's whole job is to describe an interface that does NOT exist yet, so
 * scanning it would turn "we are considering `--sql`" into a test failure. That
 * directory is currently out of scope only because `readdirSync` does not recurse
 * - which is the right outcome reached by accident, so it is written down here.
 * If this is ever made recursive, skip `proposals/` explicitly.
 */
function docFiles(): string[] {
  return readdirSync(DOCS)
    .filter((name) => name.endsWith(".md") && name !== "review-log.md")
    .map((name) => join(DOCS, name))
    .concat(EXTRA_DOCS.filter((path) => existsSync(path)));
}

function run(argv: string[]): string {
  const result = spawnSync(process.execPath, ["--import", "tsx", CLI_BIN, ...argv], {
    encoding: "utf8",
    env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
  });
  return `${result.stdout ?? ""}${result.stderr ?? ""}`;
}

test("every flag the docs name is a real flag, or is another tool's", () => {
  const named = new Map<string, string>();
  for (const file of docFiles()) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(/(?:^|[\s`(])(--[a-z][a-z0-9-]+)/gm)) {
      if (!named.has(match[1])) named.set(match[1], file);
    }
  }
  assert.ok(named.size > 15, `the scan must find flags to check; found ${named.size}`);

  // A flag restricted to another verb answers "only valid with ..." and exists.
  // Only `unknown flag` means the docs name something absent.
  const VERBS = ["apply", "status", "rollback", "resolve", "lint", "plan", "history", "new"];
  const missing: string[] = [];
  for (const [flag, file] of named) {
    if (flag in FOREIGN_FLAGS) continue;
    const known = VERBS.some((verb) => !/unknown flag/.test(run([verb, flag, "x"])));
    // Repo-relative, not the basename: four different `README.md` files are in
    // scope now, so a bare basename cannot tell you which one to open.
    if (!known) missing.push(`${flag} (named in ${relative(REPO_ROOT, file)})`);
  }
  assert.deepEqual(
    missing,
    [],
    "the docs name a flag no verb accepts - add it to FOREIGN_FLAGS only if another tool owns it",
  );
});

test("the allowlist stays honest: every foreign flag is still foreign, and still cited", () => {
  // Without this, FOREIGN_FLAGS is a place to silence the test above. An entry
  // that the CLI has since implemented must come off the list rather than sit
  // there exempting a real flag from being checked.
  const VERBS = ["apply", "status", "rollback", "resolve", "lint", "plan", "history", "new"];
  for (const [flag, owner] of Object.entries(FOREIGN_FLAGS)) {
    assert.ok(owner.length > 0, `${flag} must name the tool it belongs to`);
    const known = VERBS.some((verb) => !/unknown flag/.test(run([verb, flag, "x"])));
    assert.equal(
      known,
      false,
      `${flag} is now a real zero-migrate flag - remove it from FOREIGN_FLAGS so it is checked`,
    );
  }

  // And each must actually appear in the docs, so the list cannot accumulate
  // entries for flags nobody mentions any more.
  const allText = docFiles().map((file) => readFileSync(file, "utf8")).join("\n");
  for (const flag of Object.keys(FOREIGN_FLAGS)) {
    assert.ok(
      allText.includes(flag),
      `${flag} is no longer named in the docs - remove it from FOREIGN_FLAGS`,
    );
  }
});

test("--help lists the flags the CLI accepts, including version's --verbose", () => {
  // `--verbose` was accepted and documented in `docs/cli.md`, and absent from
  // `--help`. The mirror image of the four defects above: the docs were right
  // and the CLI's own help was the incomplete source.
  const help = run(["--help"]);
  for (const flag of ["--verbose", "--json", "--strict", "--approve", "--dialect"]) {
    assert.match(help, new RegExp(`\\s${flag}\\b`), `--help must list ${flag}`);
  }
  // And the flag it lists really works, in both forms the docs show.
  for (const argv of [["version", "--verbose"], ["--version", "--verbose"]]) {
    assert.match(
      run(argv),
      /addon version/,
      `${argv.join(" ")} must report the addon identity`,
    );
  }
});
