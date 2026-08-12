// The two MySQL preconditions for a structured data migration, measured against a
// live server rather than a mock.
//
// `docs/security-model.md` states both:
//
//   "Every insert, update, delete, and backfill target must use InnoDB."
//   "MySQL refuses structured data migrations when the target has user triggers
//    because it cannot prove that trigger side effects stay transactionally
//    consistent with the migration journal."
//
// Both guards exist (`ensure_transactional_dml_target` / `ensure_no_user_triggers`),
// and both were covered ONLY by in-module unit tests driving a recording session.
// A mock answers whatever column alias the code asks it for, so those tests prove
// the branch and not the query: they would stay green if
// `information_schema.TABLES` never returned a row under the name the guard looks
// it up by, and the guard would then fail open on a real server for the engine
// check and closed-for-the-wrong-reason for the trigger check.
//
// This is the difference the F414 lesson names - measure the path a user takes.
// Nothing here mocks a catalog: MySQL supplies the MyISAM table and the trigger.
//
// The control arm is what makes the two refusals mean anything. Without it, a
// build that refused EVERY MySQL data migration would pass both refusal arms, and
// the file would be reporting a working gate while data migrations were dead.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_dml_target";

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

function charter(database: string): string {
  const scope = `{ include = [${JSON.stringify(database)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

function authored(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

test("MySQL refuses a data migration whose target is non-InnoDB or carries a user trigger", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL data-target preconditions skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("dmltarget_my");
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  // One migration creates all three targets, so every arm below differs ONLY in
  // what was done to its table out of band.
  const created = authored("create_targets", () => {
    for (const name of ["plain_rows", "myisam_rows", "triggered_rows"]) {
      table(name).create({
        columns: { id: t.int().notNull(), stage: t.string({ length: 16 }) },
        primaryKey: ["id"],
      });
      table(name).insert({ rows: { id: 1, stage: "pending" } });
    }
  });

  const registry = {
    plain_rows: OWNER_APP,
    myisam_rows: OWNER_APP,
    triggered_rows: OWNER_APP,
  };

  const promote = (name: string) =>
    authored(`promote_${name}`, () => {
      table(name).update({
        set: { stage: "ready" },
        where: (column) => column("id").eq(1),
      });
    });

  const applyOne = (
    migration: NamedMigration,
    priors: NamedMigration[],
    reg: Record<string, string>,
  ) =>
    apply({
      migration,
      priorMigrations: priors,
      priorNameFallbacks: priors.map((p) => p.name),
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: reg,
      policy: [charter(database)],
      approved: true,
      appliedBy: "mysql-transactional-dml-target",
      nameFallback: migration.name,
    });

  /** A table's `stage`, as the DATABASE holds it. */
  const stageOf = async (name: string): Promise<string> => {
    const [rows] = await admin.query(
      `SELECT stage FROM ${mysqlIdent(database)}.${mysqlIdent(name)} WHERE id = 1`,
    );
    const list = rows as Array<{ stage: string }>;
    assert.equal(list.length, 1, `${name} holds its seeded row`);
    return list[0].stage;
  };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyOne(created, [], {});

    // The engine created these, so they start transactional and trigger-free.
    const [engines] = await admin.query(
      `SELECT TABLE_NAME AS t, ENGINE AS e FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME`,
      [database],
    );
    assert.deepEqual(
      (engines as Array<{ t: string; e: string }>).map((row) => [row.t, row.e]),
      [
        ["myisam_rows", "InnoDB"],
        ["plain_rows", "InnoDB"],
        ["triggered_rows", "InnoDB"],
      ],
      "the engine must create transactional tables to begin with",
    );

    // Out-of-band changes: exactly the two states the security model names.
    await admin.query(
      `ALTER TABLE ${mysqlIdent(database)}.myisam_rows ENGINE = MyISAM`,
    );
    await admin.query(
      `CREATE TRIGGER ${mysqlIdent(database)}.triggered_rows_audit
         BEFORE UPDATE ON ${mysqlIdent(database)}.triggered_rows
         FOR EACH ROW SET NEW.stage = NEW.stage`,
    );

    // THE CONTROL, first. An untouched InnoDB target must still apply, and the
    // row must actually move - otherwise the two refusals below prove nothing.
    const promotedPlain = promote("plain_rows");
    await applyOne(promotedPlain, [created], registry);
    assert.equal(
      await stageOf("plain_rows"),
      "ready",
      "an ordinary InnoDB target must still take its data migration",
    );

    // Both refusals run against the SAME prefix. Neither is journaled, since a
    // refused migration never completes, so they do not shadow each other.
    const prefix = [created, promotedPlain];

    // 1. Non-transactional engine. Refused before the statement runs, so the
    //    row is untouched rather than half-migrated.
    await assert.rejects(
      applyOne(promote("myisam_rows"), prefix, registry),
      /nontransactional or unsupported engine .*InnoDB/s,
      "a MyISAM target must be refused, naming the engine requirement",
    );
    assert.equal(
      await stageOf("myisam_rows"),
      "pending",
      "the refused MyISAM migration must not have mutated the row",
    );

    // 2. User trigger on an otherwise perfectly transactional target. The engine
    //    cannot prove the trigger's side effects roll back with the journal, so
    //    it fails closed and says which trigger stopped it.
    await assert.rejects(
      applyOne(promote("triggered_rows"), prefix, registry),
      /has trigger .*triggered_rows_audit.*fail closed/s,
      "a target carrying a user trigger must be refused, naming the trigger",
    );
    assert.equal(
      await stageOf("triggered_rows"),
      "pending",
      "the refused trigger migration must not have mutated the row",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(`${database}_migrations`)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
