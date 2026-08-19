// Where the MySQL text-in-key refusal stops, measured against live MySQL 8.
//
// MySQL rejects a key over a `TEXT` column with no prefix length (`ERROR 1170`).
// TWO gates refuse that shape before the deploy starts, and this file pins the
// boundary between them:
//
//   - `validate_mysql_key_storage` (offline) covers a column DECLARED IN THE SAME
//     migration as the key. It needs no connection, so it turns `lint` red in CI.
//     First arm.
//   - `validate_mysql_key_storage_for_lower` (lower time) covers a column an
//     EARLIER migration created, or one of an unmanaged table. Validation is
//     offline and reads only the migration in front of it, so those columns carry
//     no authored type; the live catalog the apply path has already introspected
//     is the only witness. Second arm.
//
// Both halves are pinned here because both were being reported wrongly. `TODO.md`
// listed the rule as unbuilt and `docs/dialects.md` said a text key "currently
// surfaces this as MySQL's apply-time error rather than an earlier validation
// error". Two documents describing a shipped gate as absent is the kind of thing
// that gets the gate rebuilt, or removed as dead code.
//
// The second arm USED to assert the mid-deploy server error, deliberately written
// to fail when that improved. It has improved, and this is the record of what
// shipped. Measured on this database before the fix, the second arm's migration
// died mid-deploy with the server's own `BLOB/TEXT column 'body' used in key
// specification without a key length`; it is now refused with nothing applied.
//
// That arm stays TWO-SIDED and must not be reduced to the refusal alone. It pins
// the refusal (a regression that lets the shape through fails here) AND a bounded
// control (a gate that starts refusing every keyed string column fails here too).
// An over-refusing gate is worse than the bug it closes, and this one came close:
// the catalog field the check reads, `mysql_physical_type`, was chosen precisely
// because the neighbouring `data_type` canonicalises `varchar(n)` to `"text"` and
// would have refused every bounded key in the project.
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

function authored(name: string, schema: () => void): NamedMigration {
  return { name, default: { schema } } as NamedMigration;
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

    for (const [label, schema] of shapes) {
      await assert.rejects(
        applyOne(authored(label.replaceAll(" ", "_"), schema)),
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

test("MySQL: a key over a column an EARLIER migration created is refused at lower time, and a bounded one still applies", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL cross-migration text-in-key skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace("textkey_my2");
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  // One first migration carrying BOTH shapes, so the refused and the permitted
  // key differ ONLY in whether the column is bounded. Anything else that diverged
  // between them would be a rival explanation for the two verdicts.
  const created = authored("create_docs", () => {
    table("docs").create({
      columns: { id: t.int().notNull(), body: t.text(), slug: t.string({ length: 120 }) },
      primaryKey: ["id"],
    });
  });
  const indexedText = authored("index_docs_body", () => {
    table("docs").index("docs_body_idx").add({ on: [{ column: "body" }] });
  });
  const indexedBounded = authored("index_docs_slug", () => {
    table("docs").index("docs_slug_idx").add({ on: [{ column: "slug" }] });
  });

  // `priors` is the lever this arm turns. Handing the earlier migration back as a
  // prior lets the ordered-history fold re-derive `docs.body`'s AUTHORED type, so
  // the refusal can come from the authored seed. Passing NO priors empties that
  // seed and leaves the live catalog as the only witness - which is the mechanism
  // this arm exists to pin, so the over-refusal control below must use that form
  // or it measures the seed and silently proves nothing about the catalog.
  const deploy = (
    migration: NamedMigration,
    priors: NamedMigration[],
    registry: Record<string, string>,
  ) =>
    apply({
      migration,
      priorMigrations: priors,
      priorNameFallbacks: priors.map((prior) => prior.name),
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry,
      policy: [charter(database)],
      approved: true,
      appliedBy: "mysql-text-key-boundary",
      nameFallback: migration.name,
    });

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await deploy(created, [], {});
    const owned = { docs: OWNER_APP };

    // SIDE ONE - the gate fires, through the LIVE CATALOG alone. No priors, so the
    // ordered-history seed is empty and `docs.body` carries no authored type; the
    // only thing that knows it is TEXT is the `information_schema.COLUMNS` read the
    // apply path already performed. The engine's own words, naming the same rule
    // the first arm names, because it IS the same rule: one sentence, two seams.
    await assert.rejects(
      deploy(indexedText, [], owned),
      /MySQL refuses a key over a TEXT or BLOB column with no prefix length/,
      "a key over an earlier migration's t.text() column must be refused before the deploy",
    );

    // ...and the same shape is refused when the ordered history IS replayed, which
    // is the shape the CLI actually deploys. Same sentence, different witness.
    await assert.rejects(
      deploy(indexedText, [created], owned),
      /MySQL refuses a key over a TEXT or BLOB column with no prefix length/,
      "the refusal must not depend on whether prior migrations were replayed",
    );

    // ...and refused means nothing ran. The failing arm this replaced left the
    // server to raise 1170 mid-deploy, so proving the index is absent is what
    // distinguishes "refused" from "attempted and failed".
    const [afterRefusal] = (await admin.query(
      `SELECT COUNT(*) AS n FROM information_schema.STATISTICS
        WHERE TABLE_SCHEMA = ? AND INDEX_NAME = 'docs_body_idx'`,
      [database],
    )) as [Array<{ n: number }>, unknown];
    assert.equal(Number(afterRefusal[0]?.n), 0, "the refused index must not exist");

    // SIDE TWO - the gate does NOT over-refuse. Same table, same earlier
    // migration, same standalone createIndex, differing only in that the column is
    // a bounded t.string({ length }). This must still reach the server and apply.
    //
    // NO PRIORS, deliberately, and this is the whole load-bearing detail: with
    // priors the authored seed answers for `slug` and the catalog classifier is
    // never consulted, so the assertion would pass no matter how wrong that
    // classifier is. Measured - a deliberately broken build that classified from
    // the canonical `data_type` (which folds `varchar(n)` into `"text"`, and so
    // refuses every bounded key) passed the seeded form of this assertion and
    // failed this one.
    await deploy(indexedBounded, [], owned);
    const [afterBounded] = (await admin.query(
      `SELECT COUNT(*) AS n FROM information_schema.STATISTICS
        WHERE TABLE_SCHEMA = ? AND INDEX_NAME = 'docs_slug_idx'`,
      [database],
    )) as [Array<{ n: number }>, unknown];
    assert.ok(
      Number(afterBounded[0]?.n) > 0,
      "a key over an earlier migration's BOUNDED column must still apply",
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
