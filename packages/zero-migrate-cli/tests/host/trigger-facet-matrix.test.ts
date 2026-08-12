// The trigger rows of the support matrix, checked against a real database.
//
// Continues the audit in `unsupported-constraints-refuse.test.ts` and
// `index-facet-matrix.test.ts`: the matrix cannot drift from `model/support.rs`,
// but nothing checks that table against a database, so a wrong cell survives every
// gate. The trigger section is the one worth checking hardest because it is the
// only section where the three dialects DISAGREE row by row - PostgreSQL alone
// supports named trigger functions, SQLite and MySQL alone take structured bodies,
// and each dialect refuses something the others allow. Asymmetry is where a
// copy-pasted capability row goes unnoticed.
//
// EACH DIALECT IS PROBED WITH THE ACTION IT SUPPORTS, and getting that wrong is
// how the first pass of this audit misread its own results. Pairing every facet
// with `execute:` makes SQLite and MySQL refuse everything - but for the ACTION,
// not for the facet under test, so seven cells read as confirmations of the matrix
// when they measured nothing about it. PostgreSQL uses `execute:`; SQLite uses a
// structured `body:`.
//
// GATE: none for the SQLite arms (an in-process file).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const OWNER_APP = "app_trigger_facet_matrix";

type TriggerArgs = Parameters<ReturnType<ReturnType<typeof table>["trigger"]>["create"]>[0];

function charter(scopeName: string): string {
  const scope = `{ include = [${JSON.stringify(scopeName)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
`;
}

/** The one action SQLite takes: a structured body. */
const body: TriggerArgs["body"] = (b) => [b.raise({ level: "ignore", message: "skip" })];

function triggerMigration(facet: string, args: TriggerArgs): MigrationModule {
  return {
    default: {
      up() {
        table("events").create({
          columns: { id: t.int().notNull(), note: t.string({ length: 32 }) },
          primaryKey: ["id"],
        });
        table("events").trigger(`events_${facet}_trg`).create(args);
      },
    },
  } as MigrationModule;
}

function applyTrigger(facet: string, args: TriggerArgs, driver: DriverConfig): Promise<unknown> {
  return apply({
    migration: triggerMigration(facet, args),
    ownerApp: OWNER_APP,
    projectSchema: "main",
    driver,
    registry: {},
    policy: [charter("main")],
    approved: true,
    appliedBy: "trigger-facet-matrix",
    nameFallback: `trig_${facet}`,
  });
}

function withSqliteFile<T>(prefix: string, run: (driver: DriverConfig, dbPath: string) => Promise<T>) {
  const work = mkdtempSync(join(HERE, `${prefix}-`));
  const dbPath = join(work, "app.db");
  return run(
    { kind: "sqlite", appPath: dbPath, journalPath: join(work, "mig.db") },
    dbPath,
  ).finally(() => rmSync(work, { recursive: true, force: true }));
}

function sqliteObjects(dbPath: string): string[] {
  let db: DatabaseSync;
  try {
    db = new DatabaseSync(dbPath);
  } catch {
    return [];
  }
  try {
    return db
      .prepare("SELECT name FROM sqlite_master")
      .all()
      .map((row: Record<string, unknown>) => row.name as string);
  } finally {
    db.close();
  }
}

test("SQLite refuses a multi-event trigger BEFORE the deploy touches the database", async () => {
  // SQLite's CREATE TRIGGER grammar takes exactly ONE event - the same constraint
  // the support table already records for MySQL, in a message sitting two lines
  // away ("MySQL CREATE TRIGGER accepts exactly one trigger event"). SQLite was
  // declared supported anyway, so the engine lowered `BEFORE INSERT OR UPDATE`,
  // emitted it, and SQLite rejected it AT THE SERVER:
  //
  //   sqlite migration statement failed: near "OR": syntax error
  //
  // That is the failure mode the validation gate exists to prevent, and the
  // damage is that it lands MID-DEPLOY: `CREATE TABLE events` has already run by
  // then, so the migration fails with the schema half-changed.
  await withSqliteFile("trig-multi", async (driver, dbPath) => {
    await assert.rejects(
      applyTrigger("multiple_events", { timing: "before", events: ["insert", "update"], forEach: "row", body }, driver),
      // The refusal must be the ENGINE's, naming the shape - not SQLite's parser
      // complaining about "OR" after the table already exists.
      /unsupported shape|UNSUPPORTED/,
      "a multi-event trigger must be refused at lower time, not by the SQLite parser",
    );
    assert.deepEqual(
      sqliteObjects(dbPath).filter((name) => name.startsWith("events")),
      [],
      "the refusal must land before CREATE TABLE, leaving nothing behind",
    );
  });
});

test("SQLite control: the single-event trigger facets the matrix declares supported still apply", async () => {
  // Without this, the refusal above also holds for a build that refused every
  // SQLite trigger. A plain single-event trigger and a WHEN predicate are both
  // declared supported and must keep working.
  for (const [facet, args] of [
    ["baseline", { timing: "before", events: ["insert"], forEach: "row", body }],
    [
      "when_predicate",
      {
        timing: "before",
        events: ["insert"],
        forEach: "row",
        when: (col: (name: string) => { isNotNull(): unknown }) => col("note").isNotNull(),
        body,
      },
    ],
  ] as ReadonlyArray<readonly [string, TriggerArgs]>) {
    await withSqliteFile(`trig-ok-${facet}`, async (driver, dbPath) => {
      await applyTrigger(facet, args, driver);
      assert.ok(
        sqliteObjects(dbPath).includes(`events_${facet}_trg`),
        `the ${facet} trigger must really exist in sqlite_master`,
      );
    });
  }
});

test("SQLite refuses the trigger facets the matrix declares unsupported", async () => {
  for (const [facet, args] of [
    ["truncate_event", { timing: "before", events: ["truncate"], forEach: "statement", body }],
    ["statement_level", { timing: "before", events: ["insert"], forEach: "statement", body }],
    ["execute_function", { timing: "before", events: ["insert"], forEach: "row", execute: "audit_fn" }],
  ] as ReadonlyArray<readonly [string, TriggerArgs]>) {
    await withSqliteFile(`trig-no-${facet}`, async (driver, dbPath) => {
      await assert.rejects(
        applyTrigger(facet, args, driver),
        /unsupported shape|UNSUPPORTED/,
        `SQLite must refuse the ${facet} facet the matrix declares No`,
      );
      assert.deepEqual(
        sqliteObjects(dbPath).filter((name) => name.startsWith("events")),
        [],
        `the refused ${facet} migration must leave nothing behind`,
      );
    });
  }
});

// ── MySQL ────────────────────────────────────────────────────────────────────
//
// This column took THREE passes to read correctly, and the first two both failed
// the same way: the probe's own trigger BODY used `raise({ level: "ignore" })`,
// which the matrix separately declares MySQL cannot render. So every MySQL cell
// refused, the whole column looked unreachable, and the natural (wrong) reading
// was that `createTrigger` is unsupported on MySQL and the four Yes/No
// distinctions in that column describe a row nobody can reach.
//
// They do not. With a body MySQL supports - an ordinary `update` statement - the
// baseline trigger APPLIES, and the five unsupported facets refuse individually.
// The column is exactly right.
//
// The lesson is worth more than the arms: WHEN PROBING ONE MATRIX CELL, EVERY
// OTHER FACET IN THE PROBE MUST BE ONE THE DIALECT SUPPORTS. A trigger carries an
// action, a timing, an event set, a for-each and optionally a WHEN, and each is
// its own matrix row - so a careless probe measures the wrong one and reports a
// confident result. That mistake produced a false GREEN across seven cells in the
// first pass and a false ALARM across six in the second.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

/** A body MySQL renders: an ordinary UPDATE against a second table. */
const mysqlBody: TriggerArgs["body"] = (b) => [b.update({ table: "audit", set: { seen: 1 } })];

function mysqlTriggerMigration(facet: string, args: TriggerArgs): MigrationModule {
  return {
    default: {
      up() {
        table("audit").create({
          columns: { id: t.int().notNull(), seen: t.int().notNull() },
          primaryKey: ["id"],
        });
        table("events").create({
          columns: { id: t.int().notNull(), note: t.string({ length: 32 }) },
          primaryKey: ["id"],
        });
        table("events").trigger(`events_${facet}_trg`).create(args);
      },
    },
  } as MigrationModule;
}

async function withMysqlDatabase<T>(
  prefix: string,
  run: (
    admin: Awaited<ReturnType<typeof import("mysql2/promise").createConnection>>,
    database: string,
    driver: DriverConfig,
  ) => Promise<T>,
): Promise<T> {
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    return await run(admin, database, { kind: "mysql", url: MYSQL_URL! });
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${database}_migrations\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
}

test("MySQL control: a structured trigger body really applies, so the refusals below mean something", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL trigger matrix skipped");
    return;
  }
  // The arm that corrects the earlier misreading. `Structured trigger body`
  // declares MySQL Yes; this proves it, by finding the trigger in the catalog.
  await withMysqlDatabase("trig_my_ok", async (admin, database, driver) => {
    await apply({
      migration: mysqlTriggerMigration("baseline", {
        timing: "before",
        events: ["insert"],
        forEach: "row",
        body: mysqlBody,
      }),
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: {},
      policy: [charter(database)],
      approved: true,
      appliedBy: "trigger-facet-matrix",
      nameFallback: "trig_my_baseline",
    });
    const [rows] = await admin.query(
      "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ?",
      [database],
    );
    assert.deepEqual(
      (rows as Array<{ TRIGGER_NAME: string }>).map((row) => row.TRIGGER_NAME),
      ["events_baseline_trg"],
      "the structured-body trigger must really exist in the MySQL catalog",
    );
  });
});

test("MySQL refuses the trigger facets the matrix declares unsupported", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL trigger matrix skipped");
    return;
  }
  for (const [facet, args] of [
    ["truncate_event", { timing: "before", events: ["truncate"], forEach: "statement", body: mysqlBody }],
    ["statement_level", { timing: "before", events: ["insert"], forEach: "statement", body: mysqlBody }],
    ["multiple_events", { timing: "before", events: ["insert", "update"], forEach: "row", body: mysqlBody }],
    ["instead_of", { timing: "insteadOf", events: ["insert"], forEach: "row", body: mysqlBody }],
    [
      "when_predicate",
      {
        timing: "before",
        events: ["insert"],
        forEach: "row",
        when: (col: (name: string) => { isNotNull(): unknown }) => col("note").isNotNull(),
        body: mysqlBody,
      },
    ],
  ] as ReadonlyArray<readonly [string, TriggerArgs]>) {
    await withMysqlDatabase(`trig_my_${facet}`, async (admin, database, driver) => {
      await assert.rejects(
        apply({
          migration: mysqlTriggerMigration(facet, args),
          ownerApp: OWNER_APP,
          projectSchema: database,
          driver,
          registry: {},
          policy: [charter(database)],
          approved: true,
          appliedBy: "trigger-facet-matrix",
          nameFallback: `trig_my_${facet}`,
        }),
        /unsupported shape|UNSUPPORTED/,
        `MySQL must refuse the ${facet} facet the matrix declares No`,
      );
      const [tables] = await admin.query(
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        [database],
      );
      assert.deepEqual(
        (tables as Array<{ TABLE_NAME: string }>).map((row) => row.TABLE_NAME),
        [],
        `the refused ${facet} migration must leave nothing behind`,
      );
    });
  }
});
