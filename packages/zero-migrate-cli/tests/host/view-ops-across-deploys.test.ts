// View operations that span two deploys - where the object was created by a migration
// that is already applied and journaled, rather than by one in the same batch.
//
// Two separate defects live here, and each dialect has exactly one of them:
//
//   dropView across deploys      MySQL FAILS          PostgreSQL works
//   createView replace across    MySQL works          PostgreSQL FAILS
//
// They have a common shape - the fold decides against a catalog snapshot it does not
// fully trust - but different causes, and fixing either one naively makes the other
// worse. Populating MySQL's empty views map to fix the drop turns the MySQL replace arm
// into the PostgreSQL failure. That is why both live in one file: the pair is the
// constraint on the fix, and splitting them hides it.
//
// The two arms that PASS are not padding. Each rules out a wrong diagnosis the failing
// arm alone would also fit - see the comments on each.
//
// WHY MySQL REFUSES. Its catalog snapshot never populates the `views` map -
// `crates/zero-migrate/src/apply/backend/mysql/drift_sql.rs:643` returns
// `Ok(SchemaSnapshot { tables, ..Default::default() })`, where the PostgreSQL and SQLite
// builders both fill theirs in. The Node apply lowering seeds its pending-schema fold
// from exactly that snapshot (`crates/zero-migrate-node/src/lower.rs:578`, folded at
// `:607`), and the fold's `DropView` arm treats an absent view as an error rather than a
// no-op (`crates/zero-migrate/src/render/fold.rs:2348`):
//
//     Op::DropView { name, .. } => {
//         if views.remove(name).is_none() {
//             return Err(FoldError::MissingView(name.clone()));
//         }
//     }
//
// The create is not in the fold either: `ops_without_completed_journal_evidence`
// (`lower.rs:601`) excludes ops that already carry journal evidence, which an applied
// migration's create does. So the drop has nowhere to find its target.
//
// WHY THE TWO MIGRATIONS MUST BE SEPARATE DEPLOYS. Within one deploy the fold re-folds
// every pending op from the base snapshot, so a create and a drop in the same batch
// resolve against each other and this passes for the wrong reason. The create has to be
// journaled complete before the drop is planned, which is what the first `apply` in each
// arm establishes.
//
// TWO HANDLERS LOOK LIKE THEY COVER THIS AND DO NOT.
// `inflight_projection_already_reflected` (`lower.rs:1381`) carries an arm for this exact
// op-and-error pair at `:1561`, and on MySQL its `!snapshot.views.contains_key(name)` test
// is unconditionally true because the map is always empty - but its call site at `:616`
// is gated on `inflight`, so it serves crash recovery only. The arm an ordinary drop
// reaches hardcodes `ProjectionGuardVerdict::NotSatisfied` for this dialect (`:643`), for
// a reason written about guarded ops absorbing a duplicate rather than about an unguarded
// drop of an object that exists.
//
// The MySQL assertion pins the CURRENT behaviour, which is believed wrong, against the
// exact message the engine produces today. A fix that lets the drop through fails here
// and has to come back and change this file deliberately.
import assert from "node:assert/strict";
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

test("MySQL: dropping a view an applied migration created is refused by the projection", async (ctx) => {
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

    // The refusal names the fold error, so a future failure for some unrelated reason
    // cannot pass as this one.
    await assert.rejects(
      applyOne(dropTheView(), database, driver, [created]),
      (error: unknown) => {
        assert.equal(
          String((error as Error)?.message ?? error),
          'failed to project pending schema after envelope "view_drop_drop": ' +
            "fold: view `" +
            VIEW +
            "` does not exist",
        );
        return true;
      },
    );

    assert.equal(
      await liveViewCount(),
      1,
      "the refusal is a planning failure, so the view outlives the migration meant to remove it",
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

/** Re-creates the view with `replace: true`, the documented way to change a view's body. */
function replaceTheView(): NamedMigration {
  return authoredMigration("view_replace", () => {
    view(VIEW).create({
      replace: true,
      as: (q) => q.from(TABLE).select(["id"]),
    });
  });
}

// The registry has to name both objects: the replacing migration's SELECT targets the
// table, and a second deploy carries no adoption for it, so an empty registry is refused
// for ownership before the fold is ever reached. That refusal is a different question
// from the one these two arms ask.
const OWNED = { [TABLE]: OWNER_APP, [VIEW]: OWNER_APP };

test("PostgreSQL: replacing a view across two deploys is refused, because the fold ignores `replace`", async (ctx) => {
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

    // `Op::CreateView` at crates/zero-migrate/src/render/fold.rs:2317 destructures with
    // `..`, which swallows `replace`, then rejects on `views.contains_key(name)`
    // unconditionally. PostgreSQL populates its catalog views, so the second deploy
    // finds the first deploy's view and refuses - even though the renderer emits
    // CREATE OR REPLACE VIEW (crates/zero-migrate/src/render/renderer.rs:566) and the
    // dialect table calls the variant portable on every engine.
    await assert.rejects(
      applyOne(replaceTheView(), schema, driver, [created], OWNED),
      (error: unknown) => {
        assert.equal(
          String((error as Error)?.message ?? error),
          'failed to project pending schema after envelope "view_replace": ' +
            "fold: view `" +
            VIEW +
            "` already exists",
        );
        return true;
      },
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

test("MySQL: replacing a view across two deploys succeeds, but only because the catalog map is empty", async (ctx) => {
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

    // The same authored migration the PostgreSQL arm above refuses. It passes here for
    // the WRONG reason: the empty catalog map means the fold's duplicate check finds
    // nothing, so ignoring `replace` costs nothing. Populating the views map to fix the
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

test("PostgreSQL: the same authored pair drops the view, so the MySQL refusal is not about views", async (ctx) => {
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
