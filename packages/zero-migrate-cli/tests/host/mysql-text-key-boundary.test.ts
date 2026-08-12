// Where the MySQL text-in-key refusal stops, measured against live MySQL 8.
//
// MySQL rejects a key over a `TEXT` column with no prefix length (`ERROR 1170`).
// `validate_mysql_key_storage` refuses that shape before the deploy starts - but
// only for a column DECLARED IN THE SAME migration as the key, because validation
// is offline and reads only the migration in front of it.
//
// Both halves are pinned here because both were being reported wrongly. `TODO.md`
// listed the rule as unbuilt and `docs/dialects.md` said a text key "currently
// surfaces this as MySQL's apply-time error rather than an earlier validation
// error" - which is true only of the half this file's second arm covers. Two
// documents describing a shipped gate as absent is the kind of thing that gets the
// gate rebuilt, or removed as dead code.
//
// The second arm asserts the CURRENT behaviour, which is a mid-deploy server error.
// It is written to fail when that improves rather than to bless it: when the gate
// moves to lower time, where the apply path already carries a live catalog, this
// arm stops matching and is the place to record what shipped.
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
const OWNER_APP = "app_text_key";

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

test("MySQL: a key over a t.text() column declared in the SAME migration is refused before the deploy", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL text-in-key boundary skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("textkey_my");
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  const applyOne = (migration: NamedMigration) =>
    apply({
      migration,
      priorMigrations: [],
      priorNameFallbacks: [],
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: {},
      policy: [charter(database)],
      approved: true,
      appliedBy: "mysql-text-key-boundary",
      nameFallback: migration.name,
    });

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);

    // Three spellings of the same mistake, all of which validation CAN see because
    // the column is declared beside the key.
    const shapes: Array<[string, () => void]> = [
      [
        "standalone createIndex",
        () => {
          table("docs").create({
            columns: { id: t.int().notNull(), body: t.text() },
            primaryKey: ["id"],
          });
          table("docs").index("docs_body_idx").add({ on: [{ column: "body" }] });
        },
      ],
      [
        "table-level unique",
        () => {
          table("docs2").create({
            columns: { id: t.int().notNull(), body: t.text() },
            primaryKey: ["id"],
            uniques: [{ name: "docs2_body_key", columns: ["body"] }],
          });
        },
      ],
      [
        "createTable inline index",
        () => {
          table("docs3").create({
            columns: { id: t.int().notNull(), body: t.text() },
            primaryKey: ["id"],
            indexes: [{ name: "docs3_body_idx", on: [{ column: "body" }] }],
          });
        },
      ],
    ];

    for (const [label, up] of shapes) {
      await assert.rejects(
        applyOne(authored(label.replaceAll(" ", "_"), up)),
        /MySQL refuses a key over a TEXT or BLOB column with no prefix length/,
        `${label}: must be refused at validate, naming the rule`,
      );
    }
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

test("MySQL: the same key over a column an EARLIER migration created still reaches the server", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL cross-migration text-in-key skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("textkey_my2");
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  const created = authored("create_docs", () => {
    table("docs").create({
      columns: { id: t.int().notNull(), body: t.text() },
      primaryKey: ["id"],
    });
  });
  const indexed = authored("index_docs_body", () => {
    table("docs").index("docs_body_idx").add({ on: [{ column: "body" }] });
  });

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);

    await apply({
      migration: created,
      priorMigrations: [],
      priorNameFallbacks: [],
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: {},
      policy: [charter(database)],
      approved: true,
      appliedBy: "mysql-text-key-boundary",
      nameFallback: created.name,
    });

    await assert.rejects(
      apply({
        migration: indexed,
        priorMigrations: [created],
        priorNameFallbacks: [created.name],
        ownerApp: OWNER_APP,
        projectSchema: database,
        driver,
        registry: { docs: OWNER_APP },
        policy: [charter(database)],
        approved: true,
        appliedBy: "mysql-text-key-boundary",
        nameFallback: indexed.name,
      }),
      // The server's own words, not the engine's. That is the whole point of this
      // arm: validation never saw the column declared, so nothing refused it and
      // MySQL raised 1170 while the migration was running.
      /BLOB\/TEXT column 'body' used in key specification without a key length/,
      "the cross-migration case still fails at apply rather than at validate",
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
