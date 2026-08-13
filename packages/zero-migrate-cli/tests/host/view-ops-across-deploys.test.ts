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

/** A table and no view, so a later drop of the view has a prior applied migration to
 *  project onto. The projection is skipped entirely for an empty history, so without
 *  this the guard is never reached and MySQL's native IF EXISTS answers instead. */
function createTableOnly(): NamedMigration {
  return authoredMigration("view_guard_priors", () => {
    table(TABLE).create({
      columns: { id: t.int().notNull(), label: t.string().notNull() },
      primaryKey: ["id"],
    });
  });
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

    // This passes because the snapshot now carries the view, so the fold resolves the
    // drop without consulting the guard at all. The guard itself is still not honoured
    // on this dialect: the fold's DropView arm destructures with `..`, which swallows
    // `existenceGuard`, and the absorbing path one layer up
    // (crates/zero-migrate-node/src/lower.rs:638) hardcodes NotSatisfied for MySQL.
    // The arm below measures the case that distinction governs.
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

/** What `ifExists` is actually FOR: a view that was never created. The arm above drops
 *  a view that EXISTS, which the populated snapshot resolves without consulting the
 *  guard, so it cannot tell whether the guard works. These two can, and they disagree
 *  with each other - which is the point.
 *
 *  The outcome turns on whether the history has a prior applied migration, because that
 *  is what decides whether the pending-schema projection runs at all. With a prior, the
 *  projection folds the drop, the fold reports the view absent, and MySQL's guard leg
 *  refuses. That is every real deployment. */
test("MySQL: an ifExists drop of a view that never existed is refused", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL absent-view guard arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewdropabsent_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    // A prior applied migration, so the projection actually runs. With an empty history
    // it is skipped and the guard is never consulted at all.
    const priors = createTableOnly();
    await applyOne(priors, database, driver, []);
    // No createView anywhere in this history, so the guard is the only thing that could
    // let the drop through.
    const outcome = await applyOne(dropTheViewIfExists(), database, driver, [priors]).then(
      () => "ran",
      (error: unknown) => String((error as Error)?.message ?? error),
    );
    assert.match(
      outcome,
      /view `view_drop_active` does not exist/,
      "the guard leg refuses on MySQL, so ifExists does not absorb a genuinely absent view",
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

/** The same drop against an EMPTY history succeeds, and the difference is not the guard.
 *  With no prior applied migration the pending-schema projection is skipped entirely, so
 *  the fold never sees the op and MySQL's native DROP VIEW IF EXISTS answers instead.
 *
 *  This arm exists because that difference cost a wrong conclusion: the first version of
 *  the arm above applied against an empty history, passed, and read as "ifExists works
 *  on MySQL now". Traced with a temporary eprintln on the projection loop, the empty
 *  history produced NO trace line at all while the one-prior history produced
 *  `fold-dropview name=view_drop_active present=false` followed by the refusal. A
 *  fixture with no priors measures a path no deployed application takes. */
test("MySQL: the same ifExists drop succeeds when no prior migration was applied", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL empty-history guard arm skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("viewdropempty_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    const outcome = await applyOne(dropTheViewIfExists(), database, driver, []).then(
      () => "ran",
      (error: unknown) => String((error as Error)?.message ?? error),
    );
    assert.equal(
      outcome,
      "ran",
      "no priors means no projection, so the fold never refuses what it never sees",
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


/** SQLite apply never builds a pending-schema projection, so an applied prior does not
 *  change which code decides the op - unlike MySQL and PostgreSQL, where a prior is
 *  exactly what turns the projection on (`verbs.rs:303` branches on
 *  `prior_envelope_json.is_empty()`).
 *
 *  `apply()` with a sqlite driver calls `applyIrSqlite`, whose `deploy_envelopes` loop
 *  re-snapshots the live catalog after every envelope, so there is no projection to
 *  refuse at. The refusal comes from the database instead, and its TEXT is the evidence:
 *  `no such view` is SQLite speaking, where the other two dialects say
 *  `failed to project pending schema`.
 *
 *  Worth pinning because it makes a tool that rebuilds a fresh database file per run and
 *  a deployment that migrates a long-lived one exercise different halves of the engine,
 *  and no count of passing migrations on the first will say anything about the second. */
test("SQLite: apply refuses an absent view at the database, with or without a prior", async () => {
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");

  const runInFreshDir = async (priors: NamedMigration[]) => {
    const dir = mkdtempSync(join(tmpdir(), "zm-viewproj-"));
    const driver: DriverConfig = {
      kind: "sqlite",
      appPath: join(dir, "app.db"),
      journalPath: join(dir, "app.migrations.db"),
    };
    try {
      for (const prior of priors) {
        await applyOne(prior, "main", driver, []);
      }
      return await applyOne(dropTheView(), "main", driver, priors).then(
        () => "ran",
        (error: unknown) => String((error as Error)?.message ?? error),
      );
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  };

  for (const [label, priors] of [
    ["with a prior", [createTableOnly()]],
    ["with an empty history", []],
  ] as const) {
    const outcome = await runInFreshDir([...priors]);
    assert.match(
      outcome,
      /no such view: view_drop_active/,
      `${label}: SQLite itself refuses, so the fold is not what rejected this`,
    );
    assert.doesNotMatch(
      outcome,
      /failed to project pending schema/,
      `${label}: SQLite apply builds no projection, so no fold refusal can appear`,
    );
  }
});

/** The PostgreSQL half of the same question the SQLite arm above answers, and it lands
 *  the other way: here an applied prior DOES decide which layer refuses.
 *
 *  `verbs.rs:303` branches on `prior_envelope_json.is_empty()` - an empty history lowers
 *  through `lower_envelope_to_plan_with_live`, which builds no pending-schema projection,
 *  and any prior lowers through `lower_ordered_envelopes_to_plans_for_apply`, which does.
 *  MySQL was measured following that branch; this arm stops PostgreSQL from being
 *  inferred from it, because a shared branch is not evidence that two backends reach it
 *  the same way - SQLite proves they need not.
 *
 *  Both histories fail, so the error TEXT carries the result: the fold names the envelope
 *  it was projecting, PostgreSQL names the relation. */
test("PostgreSQL: an applied prior moves the refusal from the database into the fold", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG projection-branch arm skipped");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();

  const runInFreshSchema = async (priors: NamedMigration[]) => {
    const schema = uniqueNamespace("viewproj_pg");
    const meta = `${schema}_migrations`;
    const driver: DriverConfig = { kind: "postgres", url: PG_URL };
    try {
      await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
      for (const prior of priors) {
        await applyOne(prior, schema, driver, []);
      }
      return await applyOne(dropTheView(), schema, driver, priors).then(
        () => "ran",
        (error: unknown) => String((error as Error)?.message ?? error),
      );
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
           DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
        )
        .catch(() => {});
    }
  };

  try {
    const withPrior = await runInFreshSchema([createTableOnly()]);
    assert.match(
      withPrior,
      /failed to project pending schema/,
      "a prior builds the projection, so the fold refuses before any SQL is sent",
    );

    const noPrior = await runInFreshSchema([]);
    assert.doesNotMatch(
      noPrior,
      /failed to project pending schema/,
      "an empty history builds no projection, so the refusal cannot come from the fold",
    );
    assert.match(
      noPrior,
      /view_drop_active/,
      "PostgreSQL still refuses the drop, naming the relation itself",
    );
  } finally {
    await client.end().catch(() => {});
  }
});

/** The dialect split for the GUARDED drop of a never-created view, which is the pair
 *  `docs/writing-migrations.md` gets to state and therefore the pair that must be
 *  measured rather than reasoned about.
 *
 *  The MySQL half is above: with a prior applied, the fold refuses. This is the other
 *  half, and it lands the opposite way — PostgreSQL ACCEPTS the same authored migration
 *  in the same shape. Without it, MySQL's refusal reads as "the fold refuses absent
 *  views", a dialect-independent claim that is false.
 *
 *  The asymmetry is not the fold disagreeing with itself. `render/fold.rs` is guard-blind
 *  for every drop by design ("an existence_guard governs only apply-time presence"), so
 *  the fold would refuse on both if both reached it with the op intact. PostgreSQL
 *  resolves the guard while LOWERING, against the snapshot, so the op is already gone by
 *  the time the history is folded. MySQL carries it through, and it dies in the fold.
 *
 *  The split is at lowering, NOT at apply. Every dialect probes at apply
 *  (`mysql/session.rs` calls `existence_probe::decide` exactly as PostgreSQL does), but
 *  projection runs first, so on MySQL the probe is never consulted here at all — which
 *  is also why the native `DROP VIEW IF EXISTS` the docs once credited MySQL with is
 *  unreachable for an absent view.
 *
 *  Both arms drop a view NO migration in the history ever created, so a pass cannot come
 *  from the snapshot resolving a known view without consulting the guard. */
test("PostgreSQL: an ifExists drop of a view that never existed is absorbed, unlike MySQL", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PG absent-view guard arm skipped");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();

  const schema = uniqueNamespace("viewdropabsent_pg");
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    // The prior is what builds the projection at all; without it this arm would be
    // measuring the empty-history path, which succeeds on every dialect and would make
    // the comparison against MySQL meaningless.
    const priors = createTableOnly();
    await applyOne(priors, schema, driver, []);

    const outcome = await applyOne(dropTheViewIfExists(), schema, driver, [priors]).then(
      () => "ran",
      (error: unknown) => String((error as Error)?.message ?? error),
    );
    assert.equal(
      outcome,
      "ran",
      "PostgreSQL resolves the guard against the catalog before projection, so an absent " +
        "view is a satisfied no-op rather than the fold error MySQL raises",
    );

    // And nothing was created to make it true.
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.views
        WHERE table_schema = $1 AND table_name = $2`,
      [schema, VIEW],
    );
    assert.equal(rows.length, 0, "the absorbed drop must not have created the view");
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
