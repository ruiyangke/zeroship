// One `zero-migrate apply` is ONE addon call over the whole directory.
//
// The addon lowers the set it is handed once and applies every migration the
// journal does not carry, in order, each committing on its own. A CLI that called
// it once per file, handing each call the prefix that ends at that file, made the
// cost of a deploy grow with the square of the directory: every call re-sent,
// re-lowered and re-reconciled everything before it, over a fresh session, with a
// fresh journal bootstrap. These arms count what crosses each boundary while the
// shipped CLI entry point runs in this process against the run's PostgreSQL:
//
//   - the addon boundary: how many `applyIr` calls one apply makes, and how many
//     envelopes they carry between them - every envelope crosses once, and is
//     lowered by the one call that receives it;
//   - the database boundary: how many sessions one apply opens, how many times it
//     bootstraps the journal and reads the live catalog, and how many statements it
//     issues in all.
//
// The CONTROL is the directory's size. Sets of two, four and six migrations of the
// same shape must each cost one call, one session and one bootstrap, with the live
// catalog read once per migration applied, and each migration added must add the
// same number of statements: linear, not a growing prefix. A rerun that applies
// nothing reads the catalog once and issues the same statements whatever the size
// of the set.
//
// Then the crash between two migrations: a failure in the second leaves the first
// committed, journaled alone and reported on its own line, and a rerun skips it and
// applies the rest exactly once. A migration inserted before one that already ran
// is refused naming both files, and an empty set is refused before a session opens.
//
// GATE: the run's PostgreSQL container (see `live-db.ts`).

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import type { Client } from "pg";

import { main } from "../../src/cli.js";
import { apply } from "../../src/index.js";
import { ADDON_PATH } from "./addon.js";
import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_one_call";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

/** What one in-process `zero-migrate` run did at the two boundaries it crosses. */
interface Crossings {
  code: number;
  addonCalls: number;
  envelopes: number;
  sessions: number;
  statements: string[];
  stdout: string;
  stderr: string;
}

type ApplyIr = (hostDriver: unknown, req: { envelopes: unknown[] }) => Promise<unknown>;

/** Run the CLI entry point in this process, counting every addon call, pg session
 *  and statement it makes. Each patch is undone before this returns. */
async function observed(argv: readonly string[]): Promise<Crossings> {
  const addon = createRequire(import.meta.url)(ADDON_PATH) as { applyIr: ApplyIr };
  const pg = (await import("pg")).default;
  const client = pg.Client.prototype as unknown as {
    connect: (...args: unknown[]) => unknown;
    query: (...args: unknown[]) => unknown;
  };
  const originals = {
    applyIr: addon.applyIr,
    connect: client.connect,
    query: client.query,
    stdout: process.stdout.write,
    stderr: process.stderr.write,
  };
  const seen = { addonCalls: 0, envelopes: 0, sessions: 0, statements: [] as string[] };
  let stdout = "";
  let stderr = "";
  addon.applyIr = function (this: unknown, hostDriver, req) {
    seen.addonCalls += 1;
    seen.envelopes += req.envelopes.length;
    return originals.applyIr.call(this, hostDriver, req);
  };
  client.connect = function (this: unknown, ...args: unknown[]) {
    seen.sessions += 1;
    return originals.connect.apply(this, args);
  };
  client.query = function (this: unknown, ...args: unknown[]) {
    const [config] = args;
    seen.statements.push(
      typeof config === "string" ? config : String((config as { text?: unknown }).text),
    );
    return originals.query.apply(this, args);
  };
  process.stdout.write = ((chunk: unknown) => {
    stdout += String(chunk);
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: unknown) => {
    stderr += String(chunk);
    return true;
  }) as typeof process.stderr.write;
  try {
    const code = await main([...argv]);
    return { code, ...seen, stdout, stderr };
  } finally {
    addon.applyIr = originals.applyIr;
    client.connect = originals.connect;
    client.query = originals.query;
    process.stdout.write = originals.stdout;
    process.stderr.write = originals.stderr;
  }
}

/** A project directory holding `migrations` as files, in the order given. */
function project(schema: string, migrations: ReadonlyArray<[string, string]>): string {
  const work = mkdtempSync(join(HERE, ".one-call-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), noInjectPolicy(schema));
  migrations.forEach(([name, body], index) => {
    const stamp = `202601${String(index + 1).padStart(2, "0")}000000`;
    writeFileSync(join(work, "migrations", `${stamp}_${name}.ts`), body);
  });
  return work;
}

function createTable(table: string): [string, string] {
  return [
    table,
    `import { table, t } from "@zeroship/migrate";
export const name = ${JSON.stringify(table)};
export default {
  schema() {
    table(${JSON.stringify(table)}).create({ columns: { id: t.int().required() }, primaryKey: ["id"] });
  },
};
`,
  ];
}

function applyArgs(work: string, schema: string, registry: Record<string, string>): string[] {
  writeFileSync(join(work, "registry.json"), JSON.stringify(registry));
  return [
    "apply",
    "--approve",
    "--dir", join(work, "migrations"),
    "--database-url", pgUrl(),
    "--schema", schema,
    "--policy", join(work, "policy.toml"),
    "--registry", join(work, "registry.json"),
    "--owner-app", OWNER_APP,
  ];
}

/** The per-file report lines, keyed by label, each parsed. */
function reportLines(stdout: string): Map<string, { applied: string[]; skipped: string[] }> {
  const lines = stdout.split("\n").filter((line) => line.startsWith("apply "));
  return new Map(
    lines.map((line) => {
      const separator = line.indexOf(": ");
      return [line.slice("apply ".length, separator), JSON.parse(line.slice(separator + 2))];
    }),
  );
}

function countMatching(statements: readonly string[], needle: string): number {
  return statements.filter((statement) => statement.includes(needle)).length;
}

async function appliedNames(admin: Client, schema: string): Promise<string[]> {
  const { rows } = await admin.query(
    `SELECT name FROM ${pgIdent(`${schema}_migrations`)}.__zeroship_schema_migrations
      WHERE event_kind = 'applied' ORDER BY event_seq`,
  );
  return rows.map((row: { name: string }) => row.name);
}

/** The journal bootstrap's first statement, and one statement the live catalog
 *  snapshot issues exactly once per read. */
const BOOTSTRAP = (schema: string) => `CREATE SCHEMA IF NOT EXISTS ${pgIdent(`${schema}_migrations`)}`;
const CATALOG_READ = "SELECT r.rolname, r.rolcanlogin";

test("apply runs the whole directory in one call that sends each migration once", async () => {
  const admin = await connectLivePg();
  const created: Array<{ schema: string; work: string }> = [];
  try {
    const fresh = new Map<number, Crossings>();
    const rerun = new Map<number, Crossings>();
    for (const size of [2, 4, 6]) {
      const schema = uniqueNamespace(`one_call_${size}`);
      const tables = Array.from({ length: size }, (_, index) => `t${index + 1}`);
      const work = project(schema, tables.map(createTable));
      created.push({ schema, work });
      await admin.query(`CREATE SCHEMA ${pgIdent(schema)}`);
      const registry = Object.fromEntries(tables.map((table) => [table, OWNER_APP]));

      const first = await observed(applyArgs(work, schema, registry));
      assert.equal(first.code, 0, `apply over ${size} files must succeed: ${first.stderr}`);
      const firstLines = reportLines(first.stdout);
      assert.equal(firstLines.size, size, `one report line per file: ${first.stdout}`);
      for (const [label, line] of firstLines) {
        assert.equal(line.applied.length, 1, `${label} applied its one step: ${first.stdout}`);
        assert.deepEqual(line.skipped, [], `${label} skipped nothing on a fresh schema`);
      }
      assert.deepEqual(
        await appliedNames(admin, schema),
        tables.map((table) => `create_table_${table}`),
        "every migration is journaled once, in file order",
      );
      fresh.set(size, first);

      const again = await observed(applyArgs(work, schema, registry));
      assert.equal(again.code, 0, `the rerun must succeed: ${again.stderr}`);
      const againLines = reportLines(again.stdout);
      assert.equal(againLines.size, size, `one report line per file on the rerun too`);
      for (const [label, line] of againLines) {
        assert.deepEqual(line.applied, [], `${label} is not applied again`);
        assert.deepEqual(line.skipped, firstLines.get(label)?.applied, `${label} reports its step skipped`);
      }
      assert.equal((await appliedNames(admin, schema)).length, size, "the rerun journals nothing");
      rerun.set(size, again);

      for (const [phase, run] of [["fresh", first], ["rerun", again]] as const) {
        assert.equal(run.addonCalls, 1, `${phase} apply of ${size} files is one addon call`);
        assert.equal(run.envelopes, size, `${phase} apply sends each of ${size} envelopes once`);
        assert.equal(run.sessions, 1, `${phase} apply of ${size} files opens one session`);
        assert.equal(
          countMatching(run.statements, BOOTSTRAP(schema)),
          1,
          `${phase} apply of ${size} files bootstraps the journal once`,
        );
      }
      // The catalog is read once at the start, and once more before each migration
      // that follows one this run committed, so every migration is lowered against
      // the database as it stands when it runs.
      assert.equal(
        countMatching(first.statements, CATALOG_READ),
        size,
        `a fresh apply of ${size} files reads the catalog once per migration it applies`,
      );
      assert.equal(
        countMatching(again.statements, CATALOG_READ),
        1,
        `a rerun of ${size} files reads the catalog once`,
      );
    }

    // Linear in the set: every migration added costs the same statements.
    const statements = (runs: Map<number, Crossings>, size: number) =>
      runs.get(size)?.statements.length ?? Number.NaN;
    const perTwo = statements(fresh, 4) - statements(fresh, 2);
    assert.ok(perTwo > 0, "two more migrations issue more statements");
    assert.equal(
      statements(fresh, 6) - statements(fresh, 4),
      perTwo,
      "each pair of migrations added costs the same statements as the pair before",
    );
    // A rerun that applies nothing does not pay per migration at all.
    assert.ok(statements(rerun, 2) > 0, "a rerun still reconciles");
    assert.equal(
      statements(rerun, 6),
      statements(rerun, 2),
      "a rerun of six files issues the statements a rerun of two does",
    );
  } finally {
    for (const { schema, work } of created) {
      await admin
        .query(
          `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
           DROP SCHEMA IF EXISTS ${pgIdent(`${schema}_migrations`)} CASCADE`,
        )
        .catch(() => {});
      rmSync(work, { recursive: true, force: true });
    }
    await admin.end().catch(() => {});
  }
});

test("a failure in the second migration leaves the first committed, and a rerun resumes", async () => {
  const admin = await connectLivePg();
  const schema = uniqueNamespace("one_call_crash");
  // The second migration inserts a row the out-of-band table already holds. No
  // lowering can see that: the failure is the database's, at execution, after the
  // first migration has committed in the same call.
  const fill: [string, string] = [
    "fill_guard",
    `import { table } from "@zeroship/migrate";
export const name = "fill_guard";
export default {
  data() {
    table("guard_rows").insert({ rows: { id: 1 } });
  },
  inverse() {
    table("guard_rows").delete({ where: (col) => col("id").eq(1) });
  },
};
`,
  ];
  const work = project(schema, [createTable("b_a"), fill, createTable("b_c")]);
  const registry = { b_a: OWNER_APP, guard_rows: OWNER_APP, b_c: OWNER_APP };
  const tables = async (): Promise<string[]> =>
    (
      await admin.query(
        `SELECT table_name FROM information_schema.tables
          WHERE table_schema = $1 AND table_name LIKE 'b\\_%' ORDER BY 1`,
        [schema],
      )
    ).rows.map((row: { table_name: string }) => row.table_name);
  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await admin.query(
      `CREATE TABLE ${pgIdent(schema)}.guard_rows (id int PRIMARY KEY);
       INSERT INTO ${pgIdent(schema)}.guard_rows (id) VALUES (1)`,
    );

    const halted = await observed(applyArgs(work, schema, registry));
    assert.equal(halted.code, 1, `the second migration must fail: ${halted.stdout}`);
    assert.equal(halted.addonCalls, 1, "the failing run is still one call");
    // The file the run committed is reported as a successful run reports it, before
    // the failure; the failing file and the one after it print nothing.
    const haltedLines = [...reportLines(halted.stdout).entries()];
    assert.equal(haltedLines.length, 1, `only the committed file reports: ${halted.stdout}`);
    assert.match(haltedLines[0][0], /_b_a$/, `the committed file is b_a: ${halted.stdout}`);
    assert.equal(haltedLines[0][1].applied.length, 1, "b_a reports the step it applied");
    assert.match(
      halted.stderr,
      /^zero-migrate: migration "fill_guard" \(mig_[a-z0-9]+\): /m,
      `the refusal is about the migration that failed: ${halted.stderr}`,
    );
    assert.doesNotMatch(
      halted.stderr,
      /authored prior migration/,
      `the run stops at the failure, not at the migration after it: ${halted.stderr}`,
    );
    assert.match(
      halted.stderr,
      /this run committed "b_a" before it, and they stay applied/,
      `names what the run committed first: ${halted.stderr}`,
    );
    assert.deepEqual(await tables(), ["b_a"], "the first committed and the third never ran");
    assert.deepEqual(
      await appliedNames(admin, schema),
      ["create_table_b_a"],
      "the journal names exactly the migration that committed",
    );

    await admin.query(`DELETE FROM ${pgIdent(schema)}.guard_rows WHERE id = 1`);
    const resumed = await observed(applyArgs(work, schema, registry));
    assert.equal(resumed.code, 0, `the rerun must finish the set: ${resumed.stderr}`);
    const lines = [...reportLines(resumed.stdout).values()];
    assert.equal(lines.length, 3, `one report line per file: ${resumed.stdout}`);
    assert.deepEqual(lines[0].applied, [], "the committed migration is skipped");
    assert.equal(lines[0].skipped.length, 1, "and reported as skipped");
    assert.equal(lines[1].applied.length, 1, "the failed one applies now");
    assert.equal(lines[2].applied.length, 1, "and so does the one after it");
    assert.deepEqual(await tables(), ["b_a", "b_c"]);
    const journal = await appliedNames(admin, schema);
    assert.equal(journal.length, 3, `one journal row per migration: ${journal.join(", ")}`);
    assert.equal(journal[0], "create_table_b_a", "the first migration's row is the original one");
    assert.equal(new Set(journal).size, 3, "no migration is journaled twice");
    const { rows } = await admin.query(`SELECT id FROM ${pgIdent(schema)}.guard_rows`);
    assert.deepEqual(rows, [{ id: 1 }], "the data migration ran exactly once");
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(`${schema}_migrations`)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("a migration inserted before an applied one is refused, naming the file that ran ahead", async () => {
  const admin = await connectLivePg();
  const schema = uniqueNamespace("one_call_order");
  const first = createTable("o_first");
  const later = createTable("o_later");
  // Applied as the first and THIRD files, so a file stamped between them arrives
  // after the later one already ran.
  const work = mkdtempSync(join(HERE, ".one-call-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(join(work, "policy.toml"), noInjectPolicy(schema));
  writeFileSync(join(work, "migrations", `20260101000000_${first[0]}.ts`), first[1]);
  writeFileSync(join(work, "migrations", `20260103000000_${later[0]}.ts`), later[1]);
  const registry = { o_first: OWNER_APP, o_inserted: OWNER_APP, o_later: OWNER_APP };
  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    const applied = await observed(applyArgs(work, schema, registry));
    assert.equal(applied.code, 0, `the first two files apply: ${applied.stderr}`);
    const before = await appliedNames(admin, schema);
    assert.deepEqual(before, ["create_table_o_first", "create_table_o_later"]);

    // CONTROL: the same set rerun is clean, so the refusal below is the insert's.
    const rerun = await observed(applyArgs(work, schema, registry));
    assert.equal(rerun.code, 0, `an unchanged rerun is clean: ${rerun.stderr}`);

    const inserted = createTable("o_inserted");
    writeFileSync(join(work, "migrations", `20260102000000_${inserted[0]}.ts`), inserted[1]);
    const refused = await observed(applyArgs(work, schema, registry));
    assert.equal(refused.code, 1, `the out-of-order insert must refuse: ${refused.stdout}`);
    assert.match(
      refused.stderr,
      /journal step mig_[a-z0-9]+ of migration "o_later" \(mig_[a-z0-9]+\) is recorded while the journal has not applied migration "o_inserted" \(mig_[a-z0-9]+\), which is authored before it/,
      `the refusal names the file that ran ahead and the one it ran ahead of: ${refused.stderr}`,
    );
    assert.doesNotMatch(
      refused.stderr,
      /was not supplied/,
      `the step's file is in the directory, so it is not reported missing: ${refused.stderr}`,
    );
    assert.deepEqual(await appliedNames(admin, schema), before, "nothing was applied");
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(`${schema}_migrations`)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});

test("an empty set is refused before any session opens", async () => {
  const admin = await connectLivePg();
  const schema = uniqueNamespace("one_call_empty");
  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    const pg = (await import("pg")).default;
    const client = pg.Client.prototype as unknown as { connect: (...args: unknown[]) => unknown };
    const connect = client.connect;
    let sessions = 0;
    client.connect = function (this: unknown, ...args: unknown[]) {
      sessions += 1;
      return connect.apply(this, args);
    };
    try {
      await assert.rejects(
        apply({
          migrations: [],
          ownerApp: OWNER_APP,
          projectSchema: schema,
          driver: { kind: "postgres", url: pgUrl() },
          policy: [noInjectPolicy(schema)],
        }),
        /apply needs at least one migration/,
      );
    } finally {
      client.connect = connect;
    }
    assert.equal(sessions, 0, "the refusal comes before a session is opened");
    const { rows } = await admin.query(
      "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
      [`${schema}_migrations`],
    );
    assert.equal(rows.length, 0, "no journal was created for a set with nothing in it");

    // CONTROL: the same call with one migration does open a session and journal.
    const [name, body] = createTable("e_one");
    const work = project(schema, [[name, body]]);
    try {
      const one = await observed(applyArgs(work, schema, { e_one: OWNER_APP }));
      assert.equal(one.code, 0, one.stderr);
      assert.equal(one.sessions, 1);
    } finally {
      rmSync(work, { recursive: true, force: true });
    }
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(`${schema}_migrations`)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
