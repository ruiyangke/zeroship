// A misspelled table-level constraint key applies clean, and the constraint is
// simply absent.
//
// `create({ foreignKeys: [...] })` is the real spelling. Write `fk`, or the very
// plausible transposition `foriegnKeys`, and the recorder accepts the call,
// records `constraints: null`, and `apply` exits 0 against a real server with the
// table present and NO foreign key on it. Referential integrity the author asked
// for is silently not there.
//
// THE TYPE LAYER DOES CATCH THIS. `tsc --noEmit` on the same source reports
//
//   error TS2353: Object literal may only specify known properties,
//   and 'fk' does not exist in type 'CreateTableArgs'.
//
// so this is not a hole in the types. It is that the RUNTIME is more permissive
// than the types, and `apply` loads migrations through tsx WITHOUT typechecking —
// so the CLI will execute a migration `tsc` would have rejected. Authors writing
// plain JS, or keeping migrations outside a typechecked project, have no layer
// that objects at all.
//
// WHY THIS IS WORTH PINNING RATHER THAN SHRUGGING AT: it contradicts the posture
// the engine holds everywhere else. `unsupported-constraints-refuse.test.ts`
// exists precisely because emitting a table WITHOUT a constraint the author
// declared is unacceptable — SQLite and MySQL refuse a table-level UNIQUE or
// CHECK rather than quietly dropping it. A constraint the author MISSPELLED is
// dropped without a word. Same outcome, opposite handling.
//
// THIS TEST PINS THE CURRENT BEHAVIOUR, WHICH IS THE BEHAVIOUR I THINK SHOULD
// CHANGE. It is written the same way as `guard-adoption-blind-spot.test.ts`: if
// the runtime learns to reject unknown keys, this test FAILS, and the failure is
// the signal to invert it rather than a regression. Do not "fix" it by loosening
// the assertion.
//
// The decision it is waiting on is a public-API one — whether the DSL should
// reject unknown `create()` keys at runtime, and whether that breaks callers
// passing extra properties deliberately — so it is the maintainer's to make.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_typo";

/** `spelling` is the key the author writes for the table-level foreign key. */
function migration(spelling: string): string {
  return `import { table, t } from "zero-migrate";
export const name = "fk_by_${spelling}";
export default {
  up() {
    table("parent").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
    table("child").create({
      columns: { id: t.int().notNull(), pid: t.int().notNull() },
      primaryKey: ["id"],
      ${spelling}: [
        { name: "child_pid_fkey", columns: ["pid"], references: { table: "parent", columns: ["id"] } },
      ],
    });
  },
};
`;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string, spelling: string): string {
  const work = mkdtempSync(join(HERE, "typo-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(
    join(work, "registry.json"),
    JSON.stringify({ parent: OWNER_APP, child: OWNER_APP }),
  );
  writeFileSync(join(work, "migrations", "20260101000000_m.ts"), migration(spelling));
  return work;
}

function apply(work: string, schema: string) {
  const child = spawn(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, "apply", "--approve",
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--schema", schema,
      "--owner-app", OWNER_APP,
    ],
    { cwd: work, env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" } },
  );
  let err = "";
  child.stderr.on("data", (chunk) => (err += chunk));
  return new Promise<{ code: number | null; err: string }>((done) =>
    child.on("close", (code) =>
      done({ code, err: err.replace(/^WARNING.*$/gm, "").trim() }),
    ),
  );
}

test("a misspelled foreign-key option applies clean and lands no constraint", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  // `foriegnKeys` is the transposition a real author makes; `fk` is the plausible
  // abbreviation. Both are silently accepted today.
  for (const spelling of ["fk", "foriegnKeys"]) {
    const schema = uniqueNamespace(`typo_${spelling.toLowerCase()}`);
    const meta = `${schema}_migrations`;
    const work = project(schema, spelling);
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      const ran = await apply(work, schema);

      assert.equal(
        ran.code,
        0,
        `apply currently SUCCEEDS with the misspelled key \`${spelling}\`; ` +
          `if it now refuses, that is the intended fix - invert this test. ${ran.err}`,
      );

      // The table is really there, so this is a silent drop and not a no-op run.
      const { rows: tables } = await client.query(
        `SELECT table_name FROM information_schema.tables
          WHERE table_schema = $1 AND table_name = 'child'`,
        [schema],
      );
      assert.equal(tables.length, 1, "the child table must actually have been created");

      // And the foreign key the author asked for is not on it.
      const { rows: fks } = await client.query(
        `SELECT con.conname
           FROM pg_constraint con
           JOIN pg_namespace n ON n.oid = con.connamespace
          WHERE n.nspname = $1 AND con.contype = 'f'`,
        [schema],
      );
      assert.equal(
        fks.length,
        0,
        `the misspelled \`${spelling}\` currently lands NO foreign key; ` +
          `if one appeared, the runtime started honouring the key and this test should be inverted`,
      );
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
           DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
        )
        .catch(() => {});
      rmSync(work, { recursive: true, force: true });
    }
  }

  // CONTROL: the correctly spelled key is honoured, so the silence above is about
  // the SPELLING and not about table-level foreign keys being unsupported. The
  // engine refuses this particular pair for a different and correct reason - the
  // reference must name an exact ordered candidate key - so what is asserted here
  // is that the correct spelling CHANGES the outcome, which is the whole point.
  const schema = uniqueNamespace("typo_real");
  const meta = `${schema}_migrations`;
  const work = project(schema, "foreignKeys");
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const ran = await apply(work, schema);
    const { rows: fks } = await client.query(
      `SELECT con.conname
         FROM pg_constraint con
         JOIN pg_namespace n ON n.oid = con.connamespace
        WHERE n.nspname = $1 AND con.contype = 'f'`,
      [schema],
    );
    assert.ok(
      ran.code === 0 && fks.length === 1,
      `the correct spelling must reach the emitter - either applying the ` +
        `constraint or refusing for a stated reason, but never silently dropping ` +
        `it like the misspellings above. exit=${ran.code} fks=${fks.length} ${ran.err}`,
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
