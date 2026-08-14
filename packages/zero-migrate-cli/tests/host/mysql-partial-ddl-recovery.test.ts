// What a half-applied MySQL migration leaves behind, and whether the repair the
// engine prints actually works.
//
// MySQL auto-commits DDL. A migration whose second statement fails cannot roll the
// first one back, so the deploy ends with the database in a state no single journal
// row describes. That is the one failure mode PostgreSQL does not have, and every
// existing MySQL fixture applies migrations that SUCCEED.
//
// The recovery machinery for it - the inflight side-table, `MarkerMismatch`,
// `recover_inflight_ddl` - was covered only by Rust unit tests driving a recording
// session. Whether a real server produces the state those tests fabricate had never
// been measured, and the sharpest question was not reachable from a unit test at
// all: the refusal tells the operator to run a specific `DELETE`, and a documented
// repair that does not work is worse than no repair, because it is followed.
//
// Four things this pins, in the order a deploy hits them:
//
//   1. the statements that DID commit are journalled - the applied half is not
//      silently lost, which is what would make the next deploy's diagnosis wrong;
//   2. the failed step leaves an inflight marker naming itself;
//   3. the NEXT deploy refuses rather than replaying, even though replaying would
//      have succeeded here - fail-closed does not get to depend on the engine
//      guessing which statements landed;
//   4. the printed `DELETE` unwedges it, and the deploy then completes.
//
// Arm 4 is also the control. Without it every assertion above holds equally for an
// engine that is simply stuck, and "refuses forever" is not the guarantee.
//
// The obstacle is a plain out-of-band table, so nothing here injects a fault: the
// server produces the interruption the same way a disconnect or a lock timeout
// would.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`. MySQL only - PostgreSQL rolls this back.

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_partial_ddl";

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

test("a half-applied MySQL migration refuses to replay, and the printed repair works", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL partial-DDL recovery skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("partialddl_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  // Two creates in ONE authored migration. The second is the one that will fail.
  const pair = {
    name: "create_pair",
    default: {
      schema() {
        table("alpha").create({
          columns: { id: t.int().notNull() },
          primaryKey: ["id"],
        });
        table("beta").create({
          columns: { id: t.int().notNull() },
          primaryKey: ["id"],
        });
      },
    },
  } as NamedMigration;

  const deploy = () =>
    apply({
      migration: pair,
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: {},
      policy: [charter(database)],
      approved: true,
      appliedBy: "mysql-partial-ddl-recovery",
      nameFallback: pair.name,
    });

  const tablesIn = async (schema: string): Promise<string[]> => {
    const [rows] = await admin.query(
      `SELECT TABLE_NAME AS t FROM information_schema.TABLES
        WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME`,
      [schema],
    );
    return (rows as Array<{ t: string }>).map((row) => row.t);
  };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    // The obstacle. `beta` already exists, so the migration's SECOND create fails
    // on a real server error rather than an injected one.
    await admin.query(`CREATE TABLE ${mysqlIdent(database)}.beta (id int PRIMARY KEY)`);

    await assert.rejects(
      deploy(),
      /Table 'beta' already exists/,
      "the second statement must fail against the pre-existing table",
    );

    // 1. The half that committed is DURABLE and RECORDED. MySQL cannot undo it, so
    //    a journal that omitted it would send the next deploy to re-create a table
    //    that is already there.
    assert.deepEqual(
      await tablesIn(database),
      ["alpha", "beta"],
      "the first create auto-committed and stands",
    );
    const [journal] = await admin.query(
      `SELECT version, name, phase FROM ${mysqlIdent(meta)}.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.deepEqual(
      (journal as Array<{ name: string; phase: string }>).map((r) => [r.name, r.phase]),
      [["create_table_alpha", "completed"]],
      "the committed step is journalled completed, and only it",
    );

    // 2. The failed step left a marker naming itself.
    const [markers] = await admin.query(
      `SELECT version, name FROM ${mysqlIdent(meta)}.schema_migrations_inflight`,
    );
    const inflight = markers as Array<{ version: string; name: string }>;
    assert.equal(inflight.length, 1, "exactly one step is inflight");
    assert.equal(inflight[0].name, "create_table_beta", "and it is the one that failed");
    const stuckVersion = inflight[0].version;

    // 3. Clear the obstacle, so a replay WOULD now succeed, and deploy again. The
    //    refusal must come anyway: the engine cannot know which statements landed,
    //    and a fail-closed that yields the moment a retry looks survivable is not
    //    fail-closed. The message must carry the version and the exact repair, or
    //    an operator holding only this text cannot act on it.
    await admin.query(`DROP TABLE ${mysqlIdent(database)}.beta`);
    await assert.rejects(
      deploy(),
      (error: Error) => {
        assert.match(error.message, /inflight marker/, "the refusal names the cause");
        assert.ok(
          error.message.includes(stuckVersion),
          `the refusal names the stuck version: ${error.message}`,
        );
        assert.ok(
          error.message.includes("schema_migrations_inflight"),
          "the refusal names the table the repair acts on",
        );
        return true;
      },
      "a marked migration must not be replayed even when the obstacle is gone",
    );
    assert.deepEqual(
      await tablesIn(database),
      ["alpha"],
      "the refused deploy must not have created anything",
    );

    // 4. The repair the message prescribes. If this does not unwedge the deploy,
    //    the engine is telling operators to do something that does not work.
    await admin.query(
      `DELETE FROM ${mysqlIdent(meta)}.schema_migrations_inflight WHERE version = ?`,
      [stuckVersion],
    );
    await deploy();
    assert.deepEqual(
      await tablesIn(database),
      ["alpha", "beta"],
      "after the prescribed repair the deploy completes",
    );
    const [settled] = await admin.query(
      `SELECT name, phase FROM ${mysqlIdent(meta)}.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.deepEqual(
      (settled as Array<{ name: string; phase: string }>).map((r) => [r.name, r.phase]),
      [
        ["create_table_alpha", "completed"],
        ["create_table_beta", "completed"],
      ],
      "and the journal ends with both steps completed, exactly once each",
    );
  } finally {
    await admin.query(`DROP DATABASE IF EXISTS ${mysqlIdent(database)}`).catch(() => {});
    await admin.query(`DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`).catch(() => {});
    await admin.end().catch(() => {});
  }
});
