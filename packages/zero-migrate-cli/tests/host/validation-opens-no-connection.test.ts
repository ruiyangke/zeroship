// `README.md`: "validation before a database connection is opened".
//
// `lint-dialect-verdicts.test.ts` covers the verdicts and says "before touching a
// database" in its header, but it never PASSES a `--database-url`. That proves
// lint tolerates the absence of a DSN; it does not prove lint ignores one it was
// given, and those are different promises. The README makes the second one.
//
// The distinction is not academic. The plausible way to break this is to make
// validation BETTER: fetch a live catalog snapshot so a rule can check something
// offline validation cannot see - exactly the direction `TODO.md` contemplates for
// the MySQL text-key rule ("the apply path already carries a live catalog
// snapshot, so the gate belongs there"). The moment any of that leaks into
// `lint`, offline validation and database-less CI both stop working, and the
// existing tests keep passing because they never supply a DSN to be dialled.
//
// So this points lint at a port with nothing behind it. Anything that opens a
// connection fails loudly with ECONNREFUSED instead of quietly.
//
// THREE ARMS, because the interesting failure is a false green:
//
//   1. an INVALID migration is refused for the dialect rule, not for a
//      connection error - the verdict is real, reached with no database;
//   2. the SAME migration passes on a dialect where the rule does not apply, so
//      arm 1 is the rule firing and not lint failing on everything;
//   3. a VALID migration exits 0 against the dead DSN, so the success path does
//      not connect either - without this, a lint that dialled the database only
//      on success would slip through.
//
// NO GATE: this test needs no database, which is the property under test.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
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

/**
 * A DSN whose host is reachable but whose port has no listener, so a connection
 * attempt fails immediately and unmistakably rather than hanging on a timeout.
 */
const DEAD_DSN = "postgres://nobody:nobody@127.0.0.1:59999/nope";

/** Keys a MySQL index on an unbounded `t.text()`, which MySQL cannot do. */
const INVALID_ON_MYSQL = `import { table, t } from "zero-migrate";
export const name = "keys_a_text_column";
export default {
  up() {
    table("docs").create({
      columns: { id: t.int().notNull(), body: t.text().notNull() },
      primaryKey: ["id"],
    });
    table("docs").index("docs_body_idx").add({ on: [{ column: "body" }] });
  },
};
`;

const VALID_EVERYWHERE = `import { table, t } from "zero-migrate";
export const name = "plain_table";
export default {
  up() {
    table("widgets").create({
      columns: { id: t.int().notNull(), label: t.string({ length: 64 }) },
      primaryKey: ["id"],
    });
  },
};
`;

function project(source: string): string {
  const work = mkdtempSync(join(HERE, "novconn-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), "policy_version = 1\n");
  writeFileSync(join(work, "registry.json"), "{}");
  writeFileSync(join(work, "migrations", "20260101000000_m.ts"), source);
  return work;
}

function lint(work: string, extra: string[]): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "lint",
      ...extra,
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--database-url", DEAD_DSN,
    ],
    {
      encoding: "utf8",
      cwd: work,
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

/** Any sign the DSN was dialled. */
const CONNECTED = /ECONNREFUSED|connection refused|ETIMEDOUT|EHOSTUNREACH|could not connect|connect ECONN/i;

test("lint reaches a real verdict against a DSN with nothing behind it", () => {
  const work = project(INVALID_ON_MYSQL);
  try {
    const run = lint(work, ["--explain", "--dialect", "mysql"]);

    assert.doesNotMatch(
      run.text,
      CONNECTED,
      `lint dialled the database. The README promises validation happens before a ` +
        `connection is opened, and database-less CI depends on it: ${run.text}`,
    );
    assert.equal(run.code, 1, `the invalid migration must be refused; ${run.text}`);
    // The verdict must be the DIALECT RULE. Without this, lint could be failing
    // for any reason at all - including a connection problem reported in words
    // this file does not match on - and the test would still pass.
    assert.match(
      run.text,
      /t\.string\(\{ length \}\)|DIALECT_UNSUPPORTED/,
      `the refusal must be the MySQL text-key rule, not an incidental failure: ${run.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("the same migration passes where the rule does not apply", () => {
  const work = project(INVALID_ON_MYSQL);
  try {
    const run = lint(work, ["--dialect", "postgres"]);
    assert.doesNotMatch(run.text, CONNECTED, `lint dialled the database: ${run.text}`);
    assert.equal(
      run.code,
      0,
      `PostgreSQL keys a text column happily, so this must pass - otherwise the ` +
        `refusal above was lint failing on everything rather than the rule firing: ${run.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("a valid migration lints clean without connecting either", () => {
  const work = project(VALID_EVERYWHERE);
  try {
    for (const dialect of ["postgres", "mysql", "sqlite"]) {
      const run = lint(work, ["--dialect", dialect]);
      assert.doesNotMatch(
        run.text,
        CONNECTED,
        `lint dialled the database on the ${dialect} success path: ${run.text}`,
      );
      assert.equal(run.code, 0, `a plain table must lint clean on ${dialect}; ${run.text}`);
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
