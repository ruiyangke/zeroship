import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  apply,
  currentIrVersion,
  status,
  type DriverConfig,
  type MigrationModule,
} from "zero-migrate-cli";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import { NO_INJECT_POLICY_CEILING } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));

if (!process.env.ZERO_MIGRATE_ADDON_PATH) {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  process.env.ZERO_MIGRATE_ADDON_PATH = join(
    HERE,
    `../../../../crates/zero-migrate-node/zero-migrate-node.${platform}-${arch}${abi}.node`,
  );
}

const PG_URL =
  process.env.ZERO_MIGRATE_TEST_PG_URL ??
  "postgres://postgres:zero_migrate@localhost:5440/zero_migrate_test";
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_complete_dml_flow";
const MIGRATION_NAME = "complete_dml_flow";
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

async function loadMigration(): Promise<MigrationModule> {
  return import("./mig/20260715000001_complete_dml_flow.ts");
}

function uniqueName(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function applyOptions(
  migration: MigrationModule,
  projectSchema: string,
  driver: DriverConfig,
  approved = true,
) {
  return {
    migration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policyCeiling: NO_INJECT_POLICY_CEILING,
    approved,
    appliedBy: "dml-e2e",
    nameFallback: MIGRATION_NAME,
  } as const;
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

async function assertAppliedPlan(
  migration: MigrationModule,
  projectSchema: string,
  driver: DriverConfig,
  appliedVersions: string[],
): Promise<void> {
  const reply = await status({
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policyCeiling: NO_INJECT_POLICY_CEILING,
    migrations: [migration],
    nameFallbacks: [MIGRATION_NAME],
  });
  assert.equal(reply.plans?.length, 1, "status returns the authored plan");
  assert.equal(reply.plans?.[0]?.state, "applied", "the complete data plan is applied");
  assert.deepEqual(
    reply.plans?.[0]?.steps.map((step) => step.name),
    STEP_NAMES,
    "status retains every authored step in order",
  );
  assert.deepEqual(
    [...(reply.plans?.[0]?.steps.map((step) => step.version) ?? [])].sort(),
    [...appliedVersions].sort(),
    "status reconciles the exact versions returned by apply",
  );
}

test("the shared migration authors the complete portable data flow", async () => {
  const migration = await loadMigration();
  const envelope = buildEnvelope(migration, {
    irVersion: currentIrVersion(),
    nameFallback: MIGRATION_NAME,
  });
  assert.equal(envelope.name, MIGRATION_NAME);
  assert.deepEqual(
    (envelope.ops as Array<{ op: string }>).map((op) => op.op),
    ["createTable", "insert", "update", "delete", "backfill"],
    "the recorder preserves the user's authored order",
  );
  const backfill = envelope.ops[4] as {
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
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  try {
    await client.connect();
  } catch {
    await client.end().catch(() => {});
    t.skip(`test Postgres unreachable at ${PG_URL} (set ZERO_MIGRATE_TEST_PG_URL)`);
    return;
  }

  const migration = await loadMigration();
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

    const first = await apply(applyOptions(migration, projectSchema, driver));
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
    await assertAppliedPlan(migration, projectSchema, driver, first.applied);

    const second = await apply(applyOptions(migration, projectSchema, driver, false));
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
  const migration = await loadMigration();
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

    const first = await apply(applyOptions(migration, projectSchema, driver));
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
    await assertAppliedPlan(migration, projectSchema, driver, first.applied);
    const journalOnly = await status({
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
    });
    assert.equal(journalOnly.plans, undefined, "journal-only status omits plan details");
    assert.deepEqual(
      [...journalOnly.applied].sort(),
      [...first.applied].sort(),
      "journal-only MySQL status reads every applied step through the MySQL backend",
    );

    const second = await apply(applyOptions(migration, projectSchema, driver, false));
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
