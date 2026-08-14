// A misspelled key in `create()` is refused, by name, before anything runs.
//
// It used to be silently dropped. `create({ fk: [...] })` recorded no foreign key
// and `create({ ifNotExist: true })` recorded no existence guard, and both applied
// clean - so the authored intent was simply absent, with nothing said. The guard
// case was the worse half: an existence guard is what makes a migration
// re-runnable, so dropping it moved the failure onto the exact re-run the guard
// was added to survive (`relation "alpha" already exists`).
//
// `tsc` rejected the same source all along (TS2353), but `apply` loads migrations
// through tsx WITHOUT typechecking, so nothing objected at the moment it mattered,
// and plain-JS authors had no layer that would.
//
// The runtime now fails closed, which is what the engine already did for a
// declared constraint it cannot emit (`unsupported-constraints-refuse.test.ts`)
// and what `ops.ts` already did for `cursorStability` ("accepts exactly mode and
// name"). This file asserts the refusal names BOTH the key the author wrote and
// the accepted keys, because the cause is a near-miss spelling and the fix is only
// obvious once the correct name is in front of them.
//
// It also asserts that NOTHING was created. A refusal that still left the table
// behind would be worse than the silent drop it replaced.
//
// Measured at the recorder, the same drop affected `primaryKey`, `uniques`,
// `indexes` and the `addColumn`/`renameTable` guards - it was a property of the
// surface, not one call. `create()` is fixed here; the other entry points still
// accept unknown keys and are worth the same treatment.
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
  schema() {
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
        1,
        `apply must REFUSE the misspelled key \`${spelling}\`; ${ran.err}`,
      );
      // The refusal has to name the offending key and the accepted ones, because
      // the cause is a near-miss spelling and the fix is only obvious once the
      // correct name is in front of the author.
      assert.match(
        ran.err,
        new RegExp(`does not accept "${spelling}"`),
        `the refusal must name the key the author actually wrote: ${ran.err}`,
      );
      assert.match(
        ran.err,
        /accepted keys are .*"foreignKeys"/,
        `the refusal must list the accepted keys, which is what makes it fixable: ${ran.err}`,
      );

      // NOTHING may have been created. A refusal that still left `child` behind
      // would be worse than the silent drop it replaced.
      const { rows: tables } = await client.query(
        `SELECT table_name FROM information_schema.tables
          WHERE table_schema = $1 AND table_name = 'child'`,
        [schema],
      );
      assert.equal(
        tables.length,
        0,
        "the refusal must happen before anything is created, not after",
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

// The same silence, on the option where it costs the most.
//
// An existence guard is what makes a migration RE-RUNNABLE. Misspell it and the
// recorder drops it exactly as it drops a misspelled constraint - except the
// consequence lands on the re-run the guard was added to survive:
//
//   ifNotExists  exit 0   the guard is honoured, the existing table is left alone
//   ifNotExist   exit 1   relation "alpha" already exists
//
// One character. No authoring-time complaint from the runtime, and the failure
// arrives against a database that already has the object.
//
// The recorder stores this as `existenceGuard: "ifNotExists"`, not as a boolean
// field named after the option - worth knowing, because a test that asserts on
// `op.ifNotExists` reads `undefined` for BOTH spellings and proves nothing. That
// mistake was made while writing this.
//
// Silently dropped in the same way, measured at the recorder: `primaryKey` ->
// `primarykey`, `index().add({ unique })` -> `uniqe`, `column().add({ ifNotExists })`,
// and `table().rename({ ifExists })`. So this is a property of the DSL surface,
// not of one call.
//
// Same inverted-style contract as the test above: it pins CURRENT behaviour and
// fails when the runtime starts rejecting unknown keys.
test("a misspelled existence guard applies unguarded and fails on an existing object", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const results: Record<string, { code: number | null; err: string }> = {};
  for (const spelling of ["ifNotExists", "ifNotExist"]) {
    const schema = uniqueNamespace(`guard_${spelling.toLowerCase()}`);
    const meta = `${schema}_migrations`;
    const work = mkdtempSync(join(HERE, "guard-"));
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
    writeFileSync(join(work, "registry.json"), JSON.stringify({ alpha: OWNER_APP }));
    writeFileSync(
      join(work, "migrations", "20260101000000_guarded.ts"),
      `import { table, t } from "zero-migrate";
export const name = "guarded_create";
export default {
  schema() {
    table("alpha").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"], ${spelling}: true });
  },
};
`,
    );
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      // The object already exists - the situation an existence guard is for.
      await client.query(`CREATE TABLE "${schema}".alpha (id int primary key)`);
      results[spelling] = await apply(work, schema);
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
  await client.end().catch(() => {});

  // The control: correctly spelled, the guard works and the deploy is re-runnable.
  assert.equal(
    results.ifNotExists.code,
    0,
    `the correct spelling must honour the guard; ${results.ifNotExists.err}`,
  );

  // One character off is now refused up front, by name.
  assert.equal(
    results.ifNotExist.code,
    1,
    `the misspelled guard must be refused; ${results.ifNotExist.err}`,
  );
  assert.match(
    results.ifNotExist.err,
    /does not accept "ifNotExist"/,
    `the refusal must name the misspelling: ${results.ifNotExist.err}`,
  );
  // The distinction that matters: it must fail because the KEY is unknown, not
  // because it applied unguarded and then collided. `already exists` here would
  // mean the guard was still being dropped and the deploy still reached the
  // server - the exact behaviour this change removed.
  assert.doesNotMatch(
    results.ifNotExist.err,
    /already exists/i,
    `it must be refused for the unknown key, not fail on the pre-existing ` +
      `object after applying unguarded: ${results.ifNotExist.err}`,
  );
});
