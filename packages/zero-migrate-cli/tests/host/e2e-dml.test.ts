import { test } from "node:test";
import assert from "node:assert/strict";

import {
  apply,
  currentIrVersion,
  status,
  type DriverConfig,
  type MigrationModule,
} from "zero-migrate-cli";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import { noInjectPolicy } from "./policy.js";
import { connectLivePg, pgUrl } from "./live-db.js";


// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = pgUrl();
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_complete_dml_flow";
const MIGRATION_NAMES = ["create_dml_flow_items", "complete_dml_flow"] as const;
const TABLE = "dml_flow_items";

const STEP_NAMES = [
  "create_table_dml_flow_items",
  "insert dml_flow_items",
  "update dml_flow_items",
  "delete dml_flow_items",
  "raise_remaining_scores",
] as const;

type VisibleRow = {
  id: number;
  label: string;
  stage: string;
  score: number;
  payloadHex: string;
};

const EXPECTED_ROWS: VisibleRow[] = [
  { id: 1, label: "alpha", stage: "ready", score: 101, payloadHex: "00017f80ff" },
  { id: 3, label: "gamma", stage: "pending", score: 103, payloadHex: "deadbeef" },
];

type BackfillProgress = {
  rowsDone: number;
  batchesDone: number;
  lastCursor: Array<{ int64: string }>;
  complete: boolean;
};

function normalizeTaggedCursor(value: unknown): BackfillProgress["lastCursor"] {
  return (typeof value === "string" ? JSON.parse(value) : value) as BackfillProgress["lastCursor"];
}

async function loadMigrations(): Promise<readonly [MigrationModule, MigrationModule]> {
  const [schema, data] = await Promise.all([
    import("./mig/20260715000000_create_dml_flow_items.ts"),
    import("./mig/20260715000001_complete_dml_flow.ts"),
  ]);
  return [schema, data];
}

function uniqueName(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function applyOptions(
  migration: MigrationModule,
  nameFallback: string,
  projectSchema: string,
  driver: DriverConfig,
  approved = true,
) {
  return {
    migration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: { [TABLE]: OWNER_APP },
    policy: [noInjectPolicy(projectSchema)],
    approved,
    appliedBy: "dml-e2e",
    nameFallback,
  } as const;
}

async function applyPair(
  migrations: readonly MigrationModule[],
  projectSchema: string,
  driver: DriverConfig,
  approved = true,
): Promise<{ applied: string[]; skipped: string[]; recovered: string[] }> {
  const outcomes = [];
  for (let index = 0; index < migrations.length; index += 1) {
    outcomes.push(
      await apply(
        applyOptions(
          migrations[index],
          MIGRATION_NAMES[index] ?? `migration_${index}`,
          projectSchema,
          driver,
          approved,
        ),
      ),
    );
  }
  return {
    applied: outcomes.flatMap((outcome) => outcome.applied),
    skipped: outcomes.flatMap((outcome) => outcome.skipped),
    recovered: outcomes.flatMap((outcome) => outcome.recovered),
  };
}

function normalizeRows(rows: Array<Record<string, unknown>>): VisibleRow[] {
  return rows.map((row) => ({
    id: Number(row.id ?? row.ID),
    label: String(row.label ?? row.LABEL),
    stage: String(row.stage ?? row.STAGE),
    score: Number(row.score ?? row.SCORE),
    payloadHex: String(row.payload_hex ?? row.PAYLOAD_HEX).toLowerCase(),
  }));
}

function assertAuthoredOrder(journalNames: string[]): void {
  const positions = STEP_NAMES.map((name) => journalNames.indexOf(name));
  for (let i = 0; i < STEP_NAMES.length; i++) {
    assert.ok(
      positions[i] >= 0,
      `journal contains ${JSON.stringify(STEP_NAMES[i])}; got ${JSON.stringify(journalNames)}`,
    );
    if (i > 0) {
      assert.ok(positions[i - 1] < positions[i], "data operations ran in authored order");
    }
  }
}

async function assertAppliedPlans(
  migrations: readonly MigrationModule[],
  projectSchema: string,
  driver: DriverConfig,
  appliedVersions: string[],
): Promise<void> {
  const reply = await status({
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: { [TABLE]: OWNER_APP },
    policy: [noInjectPolicy(projectSchema)],
    migrations: [...migrations],
    nameFallbacks: [...MIGRATION_NAMES],
  });
  assert.equal(reply.plans?.length, 2, "status returns the schema and data plans");
  assert.ok(reply.plans?.every((plan) => plan.state === "applied"), "both plans are applied");
  const steps = reply.plans?.flatMap((plan) => plan.steps) ?? [];
  assert.deepEqual(
    steps.map((step) => step.name),
    STEP_NAMES,
    "status retains every authored step in order",
  );
  assert.deepEqual(
    [...steps.map((step) => step.version)].sort(),
    [...appliedVersions].sort(),
    "status reconciles the exact versions returned by apply",
  );
}

test("the shared migration pair authors the complete portable data flow", async () => {
  const [schemaMigration, dataMigration] = await loadMigrations();
  const schemaEnvelope = buildEnvelope(schemaMigration, {
    irVersion: currentIrVersion(),
    nameFallback: MIGRATION_NAMES[0],
  });
  const dataEnvelope = buildEnvelope(dataMigration, {
    irVersion: currentIrVersion(),
    nameFallback: MIGRATION_NAMES[1],
  });
  assert.equal(schemaEnvelope.name, MIGRATION_NAMES[0]);
  assert.equal(dataEnvelope.name, MIGRATION_NAMES[1]);
  assert.deepEqual(
    [schemaEnvelope, dataEnvelope].flatMap((envelope) =>
      (envelope.ops as Array<{ op: string }>).map((op) => op.op),
    ),
    ["createTable", "insert", "update", "delete", "backfill"],
    "the recorder preserves the user's authored order",
  );
  const backfill = dataEnvelope.ops[3] as {
    cursorColumns: string[];
    cursorStability: { mode: string };
    batchSize: number;
    name: string;
  };
  assert.deepEqual(
    {
      cursorColumns: backfill.cursorColumns,
      cursorStability: backfill.cursorStability,
      batchSize: backfill.batchSize,
      name: backfill.name,
    },
    {
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 1,
      name: "raise_remaining_scores",
    },
  );
});

test("PostgreSQL: create, insert, update, delete, and backfill apply in order and rerun safely", async (t) => {
  const client = await connectLivePg(t);
  if (!client) return;

  const migrations = await loadMigrations();
  const projectSchema = uniqueName("e2e_dml_pg");
  const metaSchema = `${projectSchema}_migrations`;
  const driver = { kind: "postgres" as const, url: PG_URL };

  const readRows = async (): Promise<VisibleRow[]> => {
    const result = await client.query(
      `SELECT id, label, stage, score, encode(payload, 'hex') AS payload_hex
         FROM "${projectSchema}"."${TABLE}" ORDER BY id`,
    );
    return normalizeRows(result.rows);
  };
  const readProgress = async (): Promise<BackfillProgress> => {
    const result = await client.query(
      `SELECT rows_done, batches_done, last_cursor, complete
         FROM "${metaSchema}".schema_backfills
        WHERE name = 'raise_remaining_scores'`,
    );
    assert.equal(result.rows.length, 1, "one resumable backfill progress row exists");
    const row = result.rows[0] as Record<string, unknown>;
    return {
      rowsDone: Number(row.rows_done),
      batchesDone: Number(row.batches_done),
      lastCursor: normalizeTaggedCursor(row.last_cursor),
      complete: Boolean(row.complete),
    };
  };

  try {
    await client.query(`CREATE SCHEMA "${projectSchema}"`);

    const first = await applyPair(migrations, projectSchema, driver);
    assert.equal(first.applied.length, STEP_NAMES.length, "every authored step applied");
    assert.deepEqual(first.skipped, [], "a fresh apply skips nothing");
    assert.deepEqual(first.recovered, [], "a clean apply needs no recovery");
    assert.deepEqual(await readRows(), EXPECTED_ROWS, "the final rows reflect authored order");

    const firstProgress = await readProgress();
    assert.deepEqual(firstProgress, {
      rowsDone: 2,
      batchesDone: 2,
      lastCursor: [{ int64: "3" }],
      complete: true,
    });

    const journal = await client.query(
      `SELECT name FROM "${metaSchema}".schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journalNames = journal.rows.map((row: { name: string }) => row.name);
    assertAuthoredOrder(journalNames);
    await assertAppliedPlans(migrations, projectSchema, driver, first.applied);

    const second = await applyPair(migrations, projectSchema, driver, false);
    assert.deepEqual(second.applied, [], "an identical rerun applies nothing");
    assert.deepEqual(second.recovered, [], "an identical rerun needs no recovery");
    assert.deepEqual(
      [...second.skipped].sort(),
      [...first.applied].sort(),
      "an identical rerun skips every previously applied step without renewed approval",
    );
    assert.deepEqual(await readRows(), EXPECTED_ROWS, "an identical rerun leaves rows unchanged");
    assert.deepEqual(
      await readProgress(),
      firstProgress,
      "an identical rerun does not restart the backfill",
    );

    const journalAfterRerun = await client.query(
      `SELECT name FROM "${metaSchema}".schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.deepEqual(
      journalAfterRerun.rows,
      journal.rows,
      "an identical rerun adds no journal events",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE; DROP SCHEMA IF EXISTS "${metaSchema}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("MySQL: create, insert, update, delete, and backfill apply in order and rerun safely", async (t) => {
  if (!MYSQL_URL) {
    t.skip("ZERO_MIGRATE_MYSQL_URL unset; live MySQL e2e skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const migrations = await loadMigrations();
  const projectSchema = uniqueName("e2e_dml_mysql");
  const metaSchema = `${projectSchema}_migrations`;
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  const readRows = async (): Promise<VisibleRow[]> => {
    const [rows] = await admin.query(
      `SELECT id, label, stage, score, HEX(payload) AS payload_hex
         FROM \`${projectSchema}\`.\`${TABLE}\` ORDER BY id`,
    );
    return normalizeRows(rows as Array<Record<string, unknown>>);
  };
  const readProgress = async (): Promise<BackfillProgress> => {
    const [rows] = await admin.query(
      `SELECT rows_done, batches_done, last_cursor, complete
         FROM \`${metaSchema}\`.schema_backfills
        WHERE name = 'raise_remaining_scores'`,
    );
    const progress = rows as Array<Record<string, unknown>>;
    assert.equal(progress.length, 1, "one resumable backfill progress row exists");
    const row = progress[0];
    return {
      rowsDone: Number(row.rows_done ?? row.ROWS_DONE),
      batchesDone: Number(row.batches_done ?? row.BATCHES_DONE),
      lastCursor: normalizeTaggedCursor(row.last_cursor ?? row.LAST_CURSOR),
      complete: Number(row.complete ?? row.COMPLETE) === 1,
    };
  };

  try {
    await admin.query(`CREATE DATABASE \`${projectSchema}\``);

    const first = await applyPair(migrations, projectSchema, driver);
    assert.equal(first.applied.length, STEP_NAMES.length, "every authored step applied");
    assert.deepEqual(first.skipped, [], "a fresh apply skips nothing");
    assert.deepEqual(first.recovered, [], "a clean apply needs no recovery");
    assert.deepEqual(await readRows(), EXPECTED_ROWS, "the final rows reflect authored order");

    const firstProgress = await readProgress();
    assert.deepEqual(firstProgress, {
      rowsDone: 2,
      batchesDone: 2,
      lastCursor: [{ int64: "3" }],
      complete: true,
    });

    const [journalRows] = await admin.query(
      `SELECT name FROM \`${metaSchema}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journal = journalRows as Array<Record<string, unknown>>;
    const journalNames = journal.map((row) => String(row.name ?? row.NAME));
    assertAuthoredOrder(journalNames);
    await assertAppliedPlans(migrations, projectSchema, driver, first.applied);
    const journalOnly = await status({
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      policy: [noInjectPolicy(projectSchema)],
    });
    assert.equal(journalOnly.plans, undefined, "journal-only status omits plan details");
    assert.deepEqual(
      [...journalOnly.applied].sort(),
      [...first.applied].sort(),
      "journal-only MySQL status reads every applied step through the MySQL backend",
    );

    const second = await applyPair(migrations, projectSchema, driver, false);
    assert.deepEqual(second.applied, [], "an identical rerun applies nothing");
    assert.deepEqual(second.recovered, [], "an identical rerun needs no recovery");
    assert.deepEqual(
      [...second.skipped].sort(),
      [...first.applied].sort(),
      "an identical rerun skips every previously applied step without renewed approval",
    );
    assert.deepEqual(await readRows(), EXPECTED_ROWS, "an identical rerun leaves rows unchanged");
    assert.deepEqual(
      await readProgress(),
      firstProgress,
      "an identical rerun does not restart the backfill",
    );

    const [journalAfterRerun] = await admin.query(
      `SELECT name FROM \`${metaSchema}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.deepEqual(
      journalAfterRerun,
      journalRows,
      "an identical rerun adds no journal events",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS \`${projectSchema}\`; DROP DATABASE IF EXISTS \`${metaSchema}\``,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
