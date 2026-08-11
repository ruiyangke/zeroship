// View operations that span two deploys - where the object was created by a migration
// that is already applied and journaled, rather than by one in the same batch.
//
// Two defects were found here, and both are now fixed:
//
//                                 MySQL          PostgreSQL     SQLite
//   dropView across deploys       works          works          works
//   createView replace across     works          works          works
//
// The DROP row was FAILS on MySQL. Its catalog snapshot left the `views` map empty, the
// Node apply lowering seeds its pending-schema fold from exactly that snapshot
// (`crates/zero-migrate-node/src/lower.rs:578`, folded at `:607`), and the fold's
// `DropView` arm treats an absent view as an error rather than a no-op
// (`crates/zero-migrate/src/render/fold.rs:2348`). The deploy failed with
// `fold: view <name> does not exist`. MySQL now populates the map from
// `information_schema.VIEWS`.
//
// The REPLACE row was FAILS on PostgreSQL and SQLite until the fold learned to read
// `replace` (the `Op::CreateView` arm). MySQL passed that row even before that fix, for
// the wrong reason: with an empty views map the duplicate check had nothing to trip on.
//
// The rows stay in one file because they were coupled. Populating MySQL's views map
// while the fold still ignored `replace` would have handed MySQL the replace defect the
// other two dialects had - and the MySQL replace arm is what would have caught it.
// Fixing `replace` first removed the coupling; the arms are kept to keep it removed.
//
// WHY THE TWO MIGRATIONS MUST BE SEPARATE DEPLOYS, and why every arm here applies twice:
// within ONE deploy the fold re-folds every pending op from the base snapshot, so a
// create and a drop in the same batch resolve against each other and the arm passes
// without touching the catalog at all. The create has to be journaled complete before the
// second migration is planned - `ops_without_completed_journal_evidence`
// (`lower.rs:601`) then excludes it from `pending_ops`, leaving the catalog as the only
// place the drop can find its target. The same-migration arm below pins that distinction
// so it cannot quietly rot back.
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";

import { table, t, view } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_mysql_view_drop";
const TABLE = "view_drop_items";
const VIEW = "view_drop_active";

type NamedMigration = MigrationModule & { readonly name: string };

function authoredMigration(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

/** Creates the table and the view over it. Applied on its own so both carry completed
 *  journal evidence before the drop is planned. */
function createTableAndView(): NamedMigration {
  return authoredMigration("view_drop_create", () => {
    table(TABLE).create({
      columns: { id: t.int().notNull(), label: t.string().notNull() },
      primaryKey: ["id"],
    });
    view(VIEW).create({
      as: (q) => q.from(TABLE).select(["id", "label"]),
    });
  });
}

/** Drops the view the previous migration created. Unguarded on purpose: an `ifExists`
 *  drop asks a different question, and the guard is what a probe would absorb. */
function dropTheView(): NamedMigration {
  return authoredMigration("view_drop_drop", () => {
    view(VIEW).drop();
  });
}

function applyOne(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
  priors: NamedMigration[],
  registry: Record<string, string> = {},
) {
  return apply({
    migration,
    priorMigrations: priors,
    priorNameFallbacks: priors.map((prior) => prior.name),
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry,
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "view-drop-across-deploys-e2e",
    nameFallback: migration.name,
  });
}

test("MySQL: dropping a view an applied migration created reaches the database", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL view-drop projection skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewdrop_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  const liveViewCount = async (): Promise<number> => {
    const [rows] = await admin.query(
      `SELECT TABLE_NAME FROM information_schema.VIEWS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?`,
      [database, VIEW],
    );
    return (rows as unknown[]).length;
  };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);

    const created = createTableAndView();
    await applyOne(created, database, driver, []);
    assert.equal(await liveViewCount(), 1, "the first migration really created the view");

    await applyOne(dropTheView(), database, driver, [created]);

    assert.equal(
      await liveViewCount(),
      0,
      "the drop reaches the database rather than being refused by the projection",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("MySQL: creating and dropping a view within ONE migration succeeds, which is what makes the defect need two deploys", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL same-migration view drop skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewdrop_my1");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);

    // The create is in the same fold as the drop, so it is in `pending_ops` and the
    // empty catalog map never matters. This is the arm that would hide the defect if
    // the arm above were written this way, and it is the reason the assertion there
    // insists on a completed first deploy.
    const both = authoredMigration("view_drop_same_migration", () => {
      table(TABLE).create({
        columns: { id: t.int().notNull(), label: t.string().notNull() },
        primaryKey: ["id"],
      });
      view(VIEW).create({ as: (q) => q.from(TABLE).select(["id", "label"]) });
      view(VIEW).drop();
    });
    await applyOne(both, database, driver, []);

    const [rows] = await admin.query(
      `SELECT TABLE_NAME FROM information_schema.VIEWS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?`,
      [database, VIEW],
    );
    assert.equal(
      (rows as unknown[]).length,
      0,
      "the in-batch create satisfied the drop, and the view is gone",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

/** Re-creates the view with `replace: true`, the documented way to change a view's body.
 *
 *  Keeps the SAME projection and changes only the predicate. PostgreSQL's CREATE OR
 *  REPLACE VIEW may append columns but never remove them - narrowing (id, label) to
 *  (id) is refused by the server with "cannot drop columns from view", which is the
 *  database's rule and not this engine's. SQLite drops and recreates, so it accepts the
 *  narrowing; writing the fixture that way would have made the arms measure different
 *  things and blamed PostgreSQL for its own documented behaviour. */
function replaceTheView(): NamedMigration {
  return authoredMigration("view_replace", () => {
    view(VIEW).create({
      replace: true,
      as: (q) =>
        q
          .from(TABLE)
          .select(["id", "label"])
          .where((col) => col("label").isNotNull()),
    });
  });
}

// The registry has to name the TABLE: the replacing migration's SELECT targets it, and a
// second deploy carries no adoption for it, so an empty registry is refused for ownership
// before the fold is ever reached. That refusal is a different question from the one
// these arms ask, and a probe that stops there measures nothing about the fold.
//
// The VIEW entry is inert and kept only so the map reads as the whole authored surface.
// View names are not ownership-tracked at all: `CreateView` and `DropView` return no
// target at crates/zero-migrate-ir/src/load.rs:280, structured creation checks only the
// SOURCE tables at :310, and the registry advance tracks tables and partitions rather
// than views at crates/zero-migrate-node/src/lower.rs:1574. Whether one app should be
// able to replace another app's view is an open question, not a settled permission.
const OWNED = { [TABLE]: OWNER_APP, [VIEW]: OWNER_APP };

/** SQLite has one schema, and the CLI names it `public` in the project position. */
const SQLITE_PROJECT = "public";

test("PostgreSQL: replacing a view across two deploys applies the new body", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; view-replace arm skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace("viewrepl_pg");
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    const created = createTableAndView();
    await applyOne(created, schema, driver, []);

    // Asserting the stored definition, not merely that nothing threw: the replacing
    // migration adds a predicate the original did not have, so the new body is
    // distinguishable from the old one in the catalog.
    await applyOne(replaceTheView(), schema, driver, [created], OWNED);

    const { rows } = await client.query(
      "SELECT pg_get_viewdef(c.oid, true) AS body FROM pg_class c " +
        "JOIN pg_namespace n ON n.oid = c.relnamespace " +
        "WHERE n.nspname = $1 AND c.relname = $2",
      [schema, VIEW],
    );
    assert.equal(rows.length, 1, "the view is still there after the replace");
    assert.match(
      String(rows[0].body),
      /label IS NOT NULL/,
      "the replaced view carries the new predicate, so the replace took effect",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("MySQL: replacing a view across two deploys applies too, and did so even before the fold read `replace`", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL view-replace arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewrepl_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    const created = createTableAndView();
    await applyOne(created, database, driver, []);

    // The same authored migration the PostgreSQL arm runs. This one passed even BEFORE
    // the fold learned to read `replace`, for the wrong reason: the empty catalog map
    // meant the duplicate check found nothing, so ignoring the flag cost nothing.
    // Populating the views map to fix the
    // drop defect turns this into the PostgreSQL failure unless the fold learns to
    // consult `replace` first. This arm is what would catch that.
    await applyOne(replaceTheView(), database, driver, [created], OWNED);

    const [rows] = await admin.query(
      `SELECT VIEW_DEFINITION FROM information_schema.VIEWS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?`,
      [database, VIEW],
    );
    assert.equal((rows as unknown[]).length, 1, "the view is still there after the replace");
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

/** The guarded form of the drop. `ifExists` is the obvious thing to reach for when an
 *  unguarded drop is refused, which is why it gets measured rather than assumed. */
function dropTheViewIfExists(): NamedMigration {
  return authoredMigration("view_drop_guarded", () => {
    view(VIEW).drop({ ifExists: true });
  });
}

test("MySQL: an ifExists drop across two deploys removes the view, like the unguarded one", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL guarded view-drop arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewdropg_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    const created = createTableAndView();
    await applyOne(created, database, driver, []);

    // The fold's DropView arm destructures with `..`, which swallows `existenceGuard`
    // the way the create side swallowed `replace` until a1fe1047, so the guard never reaches
    // the decision. The guard-absorbing path that exists one layer up
    // (crates/zero-migrate-node/src/lower.rs:628-644) hardcodes NotSatisfied for this
    // dialect, so there is nowhere else for it to be honoured either.
    const outcome = await applyOne(dropTheViewIfExists(), database, driver, [created]).then(
      () => "ran",
      (error: unknown) => String((error as Error)?.message ?? error),
    );
    assert.equal(
      outcome,
      "ran",
      "the guard has a populated views map to resolve against, so the drop is planned"
    );
    const [rows] = await admin.query(
      `SELECT TABLE_NAME FROM information_schema.VIEWS
        WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?`,
      [database, VIEW],
    );
    assert.equal(
      (rows as unknown[]).length,
      0,
      "the guarded drop reaches the database rather than resolving to a no-op",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

// SQLite needs no server, so both arms always run. `dialect-table.ts:73` and `:96`
// mark the base createView and dropView variants portable here too, so this is the
// third cell of each row rather than a dialect that opts out.
test("SQLite: both view operations across two deploys", async () => {
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const dir = mkdtempSync(join(tmpdir(), "zm-viewops-"));
  const driver: DriverConfig = {
    kind: "sqlite",
    appPath: join(dir, "app.db"),
    journalPath: join(dir, "app.migrations.db"),
  };

  try {
    const created = createTableAndView();
    await applyOne(created, SQLITE_PROJECT, driver, []);

    // Recorded as whatever they are rather than asserted to match a sibling dialect:
    // the point of this arm is to find out, and a guess written as an assertion would
    // be the same mistake as inferring SQLite from PostgreSQL.
    const dropOutcome = await applyOne(dropTheView(), SQLITE_PROJECT, driver, [created]).then(
      () => "ran",
      (error: unknown) => String((error as Error)?.message ?? error),
    );
    assert.equal(dropOutcome, "ran", "SQLite drops a view an applied migration created");

    const created2 = createTableAndView();
    const dir2 = mkdtempSync(join(tmpdir(), "zm-viewops2-"));
    const driver2: DriverConfig = {
      kind: "sqlite",
      appPath: join(dir2, "app.db"),
      journalPath: join(dir2, "app.migrations.db"),
    };
    try {
      await applyOne(created2, SQLITE_PROJECT, driver2, []);
      const replaceOutcome = await applyOne(
        replaceTheView(),
        SQLITE_PROJECT,
        driver2,
        [created2],
        OWNED,
      ).then(
        () => "ran",
        (error: unknown) => String((error as Error)?.message ?? error),
      );
      assert.equal(replaceOutcome, "ran", "SQLite replaces the view like PostgreSQL does");
    } finally {
      rmSync(dir2, { recursive: true, force: true });
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("PostgreSQL: the same authored pair drops the view, the parity the MySQL arm is measured against", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; view-drop control skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace("viewdrop_pg");
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: PG_URL };

  const liveViewCount = async (): Promise<number> => {
    const { rows } = await client.query(
      "SELECT viewname FROM pg_views WHERE schemaname = $1 AND viewname = $2",
      [schema, VIEW],
    );
    return rows.length;
  };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);

    const created = createTableAndView();
    await applyOne(created, schema, driver, []);
    assert.equal(await liveViewCount(), 1, "the first migration really created the view");

    await applyOne(dropTheView(), schema, driver, [created]);
    assert.equal(
      await liveViewCount(),
      0,
      "PostgreSQL populates its catalog views, so the drop finds its target and runs",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
