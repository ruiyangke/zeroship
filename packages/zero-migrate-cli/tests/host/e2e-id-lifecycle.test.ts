// End-to-end ID lifecycle coverage over the shipped Node host seam.
//
// Every migration is authored through the public zero-migrate JavaScript API,
// lowered/applied by the real native addon, and verified against the live catalog
// and rows through the real pg/mysql2 drivers. PostgreSQL and MySQL are gated
// independently by their URL environment variables. SQLite deliberately is not
// duplicated here - a placement choice, not a missing seam: the Node host
// `DriverConfig` DOES carry `{ kind: "sqlite"; appPath; journalPath }`
// (`packages/zero-migrate-cli/src/index.ts:73`) and `apply()` routes it to
// `applyIrSqlite`. The SQLite lifecycle matrix is caught in-process by the Rust crate
// instead (`crates/zero-migrate/tests/uuid_generation.rs`,
// `ulid_value_format.rs`, `type_id_value_format.rs`,
// `synchronize_identity_sqlite.rs`). What a SQLite arm here could NOT reach either
// way is `status`, which refuses a `sqlite` driver outright
// (`packages/zero-migrate-cli/src/index.ts:409`); only
// `statusEnvelopes({ readOnly: true })` accepts one.
//
// Plan-aware status takes a live catalog snapshot before reconciliation, so the
// clean drift controls below are also a regression witness for PostgreSQL name[]
// (OID 1003) crossing the host seam as TextArray. Status exposes journal/checksum
// drift rather than StructuralDrift, so physical facet tampering is compared by a
// read-only test oracle over the same real catalogs.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  apply,
  status,
  type DriverConfig,
  type MigrationModule,
} from "zero-migrate-cli";
import { ids, table, t } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";


// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_id_lifecycle";

type NamedMigration = MigrationModule & { readonly name: string };
type PgClient = import("pg").Client;
type MysqlConnection = import("mysql2/promise").Connection;

type ForeignKeyFacet = {
  name: string;
  localColumns: string[];
  referencedTable: string;
  referencedColumns: string[];
  onDelete: string;
  onUpdate: string;
  match?: string;
};

type IdFacetSnapshot = {
  formatCheck: { name: string; definition: string } | null;
  identity: string | null;
  foreignKey: ForeignKeyFacet | null;
};

type FacetDrift = {
  table: string;
  object: string;
  field: "format" | "identity" | "reference";
  expected: unknown;
  actual: unknown;
};

function authoredMigration(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** MySQL identifiers cap at 64 chars and the matching `_migrations` DB must fit. */
function uniqueDatabase(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

function ownership(...tables: string[]): Record<string, string> {
  return Object.fromEntries(tables.map((name) => [name, OWNER_APP]));
}

async function applyInitial(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
) {
  return apply({
    migration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "id-lifecycle-e2e",
    nameFallback: migration.name,
  });
}

async function applyAfter(
  prior: NamedMigration,
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
  registry: Record<string, string>,
) {
  return apply({
    migration,
    priorMigrations: [prior],
    priorNameFallbacks: [prior.name],
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry,
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "id-lifecycle-e2e",
    nameFallback: migration.name,
  });
}

async function assertCleanStatus(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
  registry: Record<string, string>,
): Promise<void> {
  const reply = await status({
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry,
    policy: [noInjectPolicy(projectSchema)],
    migrations: [migration],
    nameFallbacks: [migration.name],
  });
  assert.equal(reply.plans?.length, 1, "status reconciles one authored plan");
  assert.equal(reply.plans?.[0]?.state, "applied", "clean plan is applied, not drifted");
  assert.ok(
    reply.plans?.[0]?.steps.every((step) => step.state === "applied"),
    "every clean plan step is applied",
  );
  assert.deepEqual(reply.pending, [], "clean status has no pending plan");
  assert.deepEqual(reply.unexpectedJournal, [], "clean status has no unexpected journal entry");
}

function compositePrimaryKeyMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_composite_primary_key", () => {
    table("composite_pk_items").create({
      columns: {
        a: t.int().notNull(),
        b: t.int().notNull(),
        payload: t.text(),
      },
      primaryKey: ["a", "b"],
    });
  });
}

function compositeForeignKeyMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_composite_foreign_key", () => {
    table("composite_fk_parents").create({
      columns: {
        a: t.int().notNull(),
        b: t.int().notNull(),
        payload: t.text(),
      },
      primaryKey: null,
      indexes: [
        {
          name: "composite_fk_parents_a_b_uq",
          on: ["a", "b"],
          unique: true,
        },
      ],
    });
    table("composite_fk_children").create({
      columns: {
        x: t.int(),
        y: t.int(),
        payload: t.text(),
      },
      foreignKeys: [
        {
          name: "composite_fk_children_parent_fk",
          columns: ["x", "y"],
          references: {
            table: "composite_fk_parents",
            columns: ["a", "b"],
          },
          onDelete: "cascade",
          onUpdate: "cascade",
        },
      ],
    });
  });
}

function primaryKeyBaseMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_primary_key_base", () => {
    table("replace_pk_items").create({
      columns: {
        id: t.int().autoIncrement().primaryKey(),
        tenant_id: t.int().notNull(),
        external_id: t.int().notNull(),
        payload: t.text(),
      },
      indexes: [
        {
          name: "replace_pk_items_tenant_external_uq",
          on: ["tenant_id", "external_id"],
          unique: true,
        },
      ],
    });
    table("add_pk_items").create({
      columns: {
        a: t.int().notNull(),
        b: t.int().notNull(),
        payload: t.text(),
      },
      primaryKey: null,
      indexes: [
        {
          name: "add_pk_items_a_b_uq",
          on: ["a", "b"],
          unique: true,
        },
      ],
    });
    table("drop_pk_items").create({
      columns: {
        a: t.int().notNull(),
        b: t.int().notNull(),
        payload: t.text(),
      },
      primaryKey: ["a", "b"],
    });
  });
}

function primaryKeyLifecycleMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_primary_key_changes", () => {
    table("replace_pk_items").primaryKey().replace({
      expectedColumns: ["id"],
      columns: ["tenant_id", "external_id"],
      dropIdentityFrom: ["id"],
    });
    table("add_pk_items").primaryKey().add({ columns: ["a", "b"] });
    table("drop_pk_items").primaryKey().drop({ expectedColumns: ["a", "b"] });
  });
}

function synchronizeIdentityBaseMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_identity_base", () => {
    table("identity_behind").create({
      columns: {
        id: t.bigInt().autoIncrement().primaryKey(),
        payload: t.text(),
      },
    });
    table("identity_ahead").create({
      columns: {
        id: t.bigInt().autoIncrement().primaryKey(),
        payload: t.text(),
      },
    });
  });
}

function synchronizeIdentityMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_synchronize_identity", () => {
    table("identity_behind").column("id").synchronizeIdentity({
      writesQuiesced: "id_lifecycle_import_window",
    });
    table("identity_ahead").column("id").synchronizeIdentity({
      writesQuiesced: "id_lifecycle_import_window",
    });
  });
}

function driftFacetMigration(): NamedMigration {
  return authoredMigration("id_lifecycle_drift_facets", () => {
    table("drift_parents").create({
      columns: {
        id: ids.typeId({ prefix: "parent" }).primaryKey(),
        payload: t.text(),
      },
    });
    table("drift_children").create({
      columns: {
        id: t.bigInt().autoIncrement().primaryKey(),
        parent_id: ids.typeId({ prefix: "parent" }).references(
          "drift_parents",
          "id",
          { onDelete: "cascade", onUpdate: "cascade" },
        ),
      },
    });
  });
}

async function pgPrimaryKeyColumns(
  client: PgClient,
  schema: string,
  tableName: string,
): Promise<string[]> {
  const result = await client.query(
    `SELECT att.attname::text AS column_name
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class tbl ON tbl.oid = con.conrelid
       JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
       CROSS JOIN LATERAL unnest(con.conkey)
         WITH ORDINALITY AS key(attnum, ordinality)
       JOIN pg_catalog.pg_attribute att
         ON att.attrelid = tbl.oid AND att.attnum = key.attnum
      WHERE ns.nspname = $1 AND tbl.relname = $2 AND con.contype = 'p'
      ORDER BY key.ordinality`,
    [schema, tableName],
  );
  return result.rows.map((row: { column_name: string }) => row.column_name);
}

async function mysqlPrimaryKeyColumns(
  connection: MysqlConnection,
  database: string,
  tableName: string,
): Promise<string[]> {
  const [rows] = await connection.query(
    `SELECT COLUMN_NAME AS column_name
       FROM information_schema.STATISTICS
      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = 'PRIMARY'
      ORDER BY SEQ_IN_INDEX`,
    [database, tableName],
  );
  return (rows as Array<Record<string, unknown>>).map((row) =>
    String(row.column_name ?? row.COLUMN_NAME),
  );
}

async function pgIdentityKind(
  client: PgClient,
  schema: string,
  tableName: string,
  columnName: string,
): Promise<string | null> {
  const result = await client.query(
    `SELECT att.attidentity::text AS identity_kind
       FROM pg_catalog.pg_attribute att
       JOIN pg_catalog.pg_class tbl ON tbl.oid = att.attrelid
       JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
      WHERE ns.nspname = $1 AND tbl.relname = $2 AND att.attname = $3
        AND att.attnum > 0 AND NOT att.attisdropped`,
    [schema, tableName, columnName],
  );
  assert.equal(result.rows.length, 1, `one catalog row for ${tableName}.${columnName}`);
  const kind = String((result.rows[0] as { identity_kind: string }).identity_kind);
  if (kind === "d") return "by default";
  if (kind === "a") return "always";
  return null;
}

async function mysqlIdentityKind(
  connection: MysqlConnection,
  database: string,
  tableName: string,
  columnName: string,
): Promise<string | null> {
  const [rows] = await connection.query(
    `SELECT EXTRA AS extra
       FROM information_schema.COLUMNS
      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?`,
    [database, tableName, columnName],
  );
  const values = rows as Array<Record<string, unknown>>;
  assert.equal(values.length, 1, `one catalog row for ${tableName}.${columnName}`);
  const extra = String(values[0].extra ?? values[0].EXTRA);
  return extra
    .split(/\s+/u)
    .some((part) => part.toLowerCase() === "auto_increment")
    ? "auto_increment"
    : null;
}

async function pgForeignKeyFacet(
  client: PgClient,
  schema: string,
  tableName: string,
  constraintName?: string,
): Promise<ForeignKeyFacet | null> {
  const nameFilter = constraintName === undefined ? "" : " AND con.conname = $3";
  const params = constraintName === undefined
    ? [schema, tableName]
    : [schema, tableName, constraintName];
  const result = await client.query(
    `SELECT con.conname::text AS constraint_name,
            local_att.attname::text AS local_column,
            ref_tbl.relname::text AS referenced_table,
            ref_att.attname::text AS referenced_column,
            key.ordinality::int AS ordinal_position,
            con.confdeltype::text AS delete_action,
            con.confupdtype::text AS update_action,
            con.confmatchtype::text AS match_type
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class local_tbl ON local_tbl.oid = con.conrelid
       JOIN pg_catalog.pg_namespace local_ns ON local_ns.oid = local_tbl.relnamespace
       JOIN pg_catalog.pg_class ref_tbl ON ref_tbl.oid = con.confrelid
       CROSS JOIN LATERAL unnest(con.conkey)
         WITH ORDINALITY AS key(local_attnum, ordinality)
       JOIN pg_catalog.pg_attribute local_att
         ON local_att.attrelid = local_tbl.oid AND local_att.attnum = key.local_attnum
       JOIN pg_catalog.pg_attribute ref_att
         ON ref_att.attrelid = ref_tbl.oid AND ref_att.attnum = con.confkey[key.ordinality]
      WHERE local_ns.nspname = $1 AND local_tbl.relname = $2
        AND con.contype = 'f'${nameFilter}
      ORDER BY key.ordinality`,
    params,
  );
  if (result.rows.length === 0) return null;
  const rows = result.rows as Array<Record<string, unknown>>;
  assert.equal(
    new Set(rows.map((row) => String(row.constraint_name))).size,
    1,
    `exactly one foreign key selected on ${tableName}`,
  );
  const action = (value: unknown): string =>
    ({ a: "NO ACTION", r: "RESTRICT", c: "CASCADE", n: "SET NULL", d: "SET DEFAULT" })[
      String(value)
    ] ?? String(value);
  return {
    name: String(rows[0].constraint_name),
    localColumns: rows.map((row) => String(row.local_column)),
    referencedTable: String(rows[0].referenced_table),
    referencedColumns: rows.map((row) => String(row.referenced_column)),
    onDelete: action(rows[0].delete_action),
    onUpdate: action(rows[0].update_action),
    match: String(rows[0].match_type) === "s" ? "SIMPLE" : String(rows[0].match_type),
  };
}

async function mysqlForeignKeyFacet(
  connection: MysqlConnection,
  database: string,
  tableName: string,
  constraintName?: string,
): Promise<ForeignKeyFacet | null> {
  const nameFilter = constraintName === undefined ? "" : " AND kcu.CONSTRAINT_NAME = ?";
  const params = constraintName === undefined
    ? [database, tableName]
    : [database, tableName, constraintName];
  const [rows] = await connection.query(
    `SELECT kcu.CONSTRAINT_NAME AS constraint_name,
            kcu.COLUMN_NAME AS local_column,
            kcu.REFERENCED_TABLE_NAME AS referenced_table,
            kcu.REFERENCED_COLUMN_NAME AS referenced_column,
            kcu.ORDINAL_POSITION AS ordinal_position,
            rc.DELETE_RULE AS delete_rule,
            rc.UPDATE_RULE AS update_rule
       FROM information_schema.KEY_COLUMN_USAGE kcu
       JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
         ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA
        AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
        AND rc.TABLE_NAME = kcu.TABLE_NAME
      WHERE kcu.CONSTRAINT_SCHEMA = ? AND kcu.TABLE_NAME = ?
        ${nameFilter}
      ORDER BY kcu.ORDINAL_POSITION`,
    params,
  );
  const values = rows as Array<Record<string, unknown>>;
  if (values.length === 0) return null;
  assert.equal(
    new Set(values.map((row) => String(row.constraint_name ?? row.CONSTRAINT_NAME))).size,
    1,
    `exactly one foreign key selected on ${tableName}`,
  );
  return {
    name: String(values[0].constraint_name ?? values[0].CONSTRAINT_NAME),
    localColumns: values.map((row) => String(row.local_column ?? row.LOCAL_COLUMN)),
    referencedTable: String(values[0].referenced_table ?? values[0].REFERENCED_TABLE),
    referencedColumns: values.map((row) =>
      String(row.referenced_column ?? row.REFERENCED_COLUMN),
    ),
    onDelete: String(values[0].delete_rule ?? values[0].DELETE_RULE),
    onUpdate: String(values[0].update_rule ?? values[0].UPDATE_RULE),
  };
}

async function pgIdentityState(
  client: PgClient,
  schema: string,
  tableName: string,
  columnName: string,
): Promise<{ lastValue: bigint; increment: bigint }> {
  const result = await client.query(
    `SELECT sequences.last_value::text AS last_value,
            sequences.increment_by::text AS increment_by
       FROM pg_catalog.pg_sequences sequences
      WHERE pg_catalog.to_regclass(
              pg_catalog.format('%I.%I', sequences.schemaname, sequences.sequencename)
            ) = pg_catalog.pg_get_serial_sequence(
              pg_catalog.format('%I.%I', $1::text, $2::text), $3::text
            )::regclass`,
    [schema, tableName, columnName],
  );
  assert.equal(result.rows.length, 1, `one owned sequence for ${tableName}.${columnName}`);
  const row = result.rows[0] as { last_value: string; increment_by: string };
  return { lastValue: BigInt(row.last_value), increment: BigInt(row.increment_by) };
}

async function mysqlIdentityCounter(
  connection: MysqlConnection,
  database: string,
  tableName: string,
): Promise<bigint> {
  const [rows] = await connection.query(
    `SELECT CAST(AUTO_INCREMENT AS CHAR) AS auto_increment
       FROM information_schema.TABLES
      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'BASE TABLE'`,
    [database, tableName],
  );
  const values = rows as Array<Record<string, unknown>>;
  assert.equal(values.length, 1, `one table counter for ${tableName}`);
  return BigInt(String(values[0].auto_increment ?? values[0].AUTO_INCREMENT));
}

function diffIdFacets(expected: IdFacetSnapshot, actual: IdFacetSnapshot): FacetDrift[] {
  const drift: FacetDrift[] = [];
  if (JSON.stringify(expected.formatCheck) !== JSON.stringify(actual.formatCheck)) {
    drift.push({
      table: "drift_parents",
      object: "column id",
      field: "format",
      expected: expected.formatCheck,
      actual: actual.formatCheck,
    });
  }
  if (expected.identity !== actual.identity) {
    drift.push({
      table: "drift_children",
      object: "column id",
      field: "identity",
      expected: expected.identity,
      actual: actual.identity,
    });
  }
  if (JSON.stringify(expected.foreignKey) !== JSON.stringify(actual.foreignKey)) {
    drift.push({
      table: "drift_children",
      object: `constraint ${expected.foreignKey?.name ?? "<missing>"}`,
      field: "reference",
      expected: expected.foreignKey,
      actual: actual.foreignKey,
    });
  }
  return drift;
}

async function pgIdFacetSnapshot(client: PgClient, schema: string): Promise<IdFacetSnapshot> {
  const checks = await client.query(
    `SELECT con.conname::text AS constraint_name,
            pg_catalog.pg_get_constraintdef(con.oid, true) AS definition
       FROM pg_catalog.pg_constraint con
       JOIN pg_catalog.pg_class tbl ON tbl.oid = con.conrelid
       JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace
       JOIN pg_catalog.pg_attribute att
         ON att.attrelid = tbl.oid AND att.attnum = con.conkey[1]
      WHERE ns.nspname = $1 AND tbl.relname = 'drift_parents'
        AND att.attname = 'id' AND con.contype = 'c'
        AND pg_catalog.array_length(con.conkey, 1) = 1
      ORDER BY con.conname`,
    [schema],
  );
  assert.ok(checks.rows.length <= 1, "at most one TypeID format CHECK is recovered");
  const formatCheck = checks.rows[0]
    ? {
        name: String(checks.rows[0].constraint_name),
        definition: String(checks.rows[0].definition),
      }
    : null;
  return {
    formatCheck,
    identity: await pgIdentityKind(client, schema, "drift_children", "id"),
    foreignKey: await pgForeignKeyFacet(
      client,
      schema,
      "drift_children",
    ),
  };
}

async function mysqlIdFacetSnapshot(
  connection: MysqlConnection,
  database: string,
): Promise<IdFacetSnapshot> {
  const [rows] = await connection.query(
    `SELECT tc.CONSTRAINT_NAME AS constraint_name,
            cc.CHECK_CLAUSE AS definition
       FROM information_schema.TABLE_CONSTRAINTS tc
       JOIN information_schema.CHECK_CONSTRAINTS cc
         ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA
        AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
      WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = 'drift_parents'
        AND tc.CONSTRAINT_TYPE = 'CHECK'
      ORDER BY tc.CONSTRAINT_NAME`,
    [database],
  );
  const checks = rows as Array<Record<string, unknown>>;
  assert.ok(checks.length <= 1, "at most one TypeID format CHECK is recovered");
  const formatCheck = checks[0]
    ? {
        name: String(checks[0].constraint_name ?? checks[0].CONSTRAINT_NAME),
        definition: String(checks[0].definition ?? checks[0].DEFINITION),
      }
    : null;
  return {
    formatCheck,
    identity: await mysqlIdentityKind(connection, database, "drift_children", "id"),
    foreignKey: await mysqlForeignKeyFacet(
      connection,
      database,
      "drift_children",
    ),
  };
}

test("PostgreSQL composite primary key is ordered and enforced", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL composite-PK e2e skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueSchema("idlife_pk_pg");
  const meta = `${schema}_migrations`;
  const migration = compositePrimaryKeyMigration();
  const driver = { kind: "postgres" as const, url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await applyInitial(migration, schema, driver);
    assert.deepEqual(
      await pgPrimaryKeyColumns(client, schema, "composite_pk_items"),
      ["a", "b"],
    );
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.composite_pk_items (a, b, payload)
       VALUES (1, 2, 'first')`,
    );
    await assert.rejects(
      client.query(
        `INSERT INTO ${pgIdent(schema)}.composite_pk_items (a, b, payload)
         VALUES (1, 2, 'duplicate')`,
      ),
      /duplicate|unique/i,
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

test("MySQL composite primary key is ordered and enforced", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL composite-PK e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("idlife_pk_my");
  const meta = `${database}_migrations`;
  const migration = compositePrimaryKeyMigration();
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(migration, database, driver);
    assert.deepEqual(
      await mysqlPrimaryKeyColumns(admin, database, "composite_pk_items"),
      ["a", "b"],
    );
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.composite_pk_items (a, b, payload)
       VALUES (1, 2, 'first')`,
    );
    await assert.rejects(
      admin.query(
        `INSERT INTO ${mysqlIdent(database)}.composite_pk_items (a, b, payload)
         VALUES (1, 2, 'duplicate')`,
      ),
      /duplicate/i,
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

test("PostgreSQL composite foreign key preserves tuple/action and MATCH SIMPLE", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL composite-FK e2e skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueSchema("idlife_fk_pg");
  const meta = `${schema}_migrations`;
  const migration = compositeForeignKeyMigration();
  const driver = { kind: "postgres" as const, url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await applyInitial(migration, schema, driver);
    assert.deepEqual(
      await pgForeignKeyFacet(
        client,
        schema,
        "composite_fk_children",
        "composite_fk_children_parent_fk",
      ),
      {
        name: "composite_fk_children_parent_fk",
        localColumns: ["x", "y"],
        referencedTable: "composite_fk_parents",
        referencedColumns: ["a", "b"],
        onDelete: "CASCADE",
        onUpdate: "CASCADE",
        match: "SIMPLE",
      },
    );
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.composite_fk_parents (a, b) VALUES (10, 20)`,
    );
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.composite_fk_children (x, y, payload)
       VALUES (10, 20, 'valid')`,
    );
    await assert.rejects(
      client.query(
        `INSERT INTO ${pgIdent(schema)}.composite_fk_children (x, y, payload)
         VALUES (10, 999, 'orphan')`,
      ),
      /foreign key/i,
    );
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.composite_fk_children (x, y, payload)
       VALUES (10, NULL, 'match-simple-null')`,
    );
    const children = await client.query(
      `SELECT count(*)::int AS n FROM ${pgIdent(schema)}.composite_fk_children`,
    );
    assert.equal(children.rows[0].n, 2, "valid and one-NULL child rows exist");
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

test("MySQL composite foreign key preserves tuple/action and MATCH SIMPLE", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL composite-FK e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("idlife_fk_my");
  const meta = `${database}_migrations`;
  const migration = compositeForeignKeyMigration();
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(migration, database, driver);
    assert.deepEqual(
      await mysqlForeignKeyFacet(
        admin,
        database,
        "composite_fk_children",
        "composite_fk_children_parent_fk",
      ),
      {
        name: "composite_fk_children_parent_fk",
        localColumns: ["x", "y"],
        referencedTable: "composite_fk_parents",
        referencedColumns: ["a", "b"],
        onDelete: "CASCADE",
        onUpdate: "CASCADE",
      },
    );
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.composite_fk_parents (a, b) VALUES (10, 20)`,
    );
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.composite_fk_children (x, y, payload)
       VALUES (10, 20, 'valid')`,
    );
    await assert.rejects(
      admin.query(
        `INSERT INTO ${mysqlIdent(database)}.composite_fk_children (x, y, payload)
         VALUES (10, 999, 'orphan')`,
      ),
      /foreign key|constraint/i,
    );
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.composite_fk_children (x, y, payload)
       VALUES (10, NULL, 'match-simple-null')`,
    );
    const [rows] = await admin.query(
      `SELECT count(*) AS n FROM ${mysqlIdent(database)}.composite_fk_children`,
    );
    assert.equal(
      Number((rows as Array<Record<string, unknown>>)[0].n),
      2,
      "valid and one-NULL child rows exist",
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

test("PostgreSQL primary-key replace/add/drop changes live keys and removes identity", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL PK-lifecycle e2e skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueSchema("idlife_altpk_pg");
  const meta = `${schema}_migrations`;
  const base = primaryKeyBaseMigration();
  const lifecycle = primaryKeyLifecycleMigration();
  const registry = ownership("replace_pk_items", "add_pk_items", "drop_pk_items");
  const driver = { kind: "postgres" as const, url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await applyInitial(base, schema, driver);
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.replace_pk_items
         (tenant_id, external_id, payload)
       VALUES (1, 101, 'first'), (1, 102, 'second');
       INSERT INTO ${pgIdent(schema)}.add_pk_items (a, b, payload)
       VALUES (2, 201, 'add');
       INSERT INTO ${pgIdent(schema)}.drop_pk_items (a, b, payload)
       VALUES (3, 301, 'drop')`,
    );
    const before = await client.query(
      `SELECT id, tenant_id, external_id, payload
         FROM ${pgIdent(schema)}.replace_pk_items ORDER BY id`,
    );

    const outcome = await applyAfter(base, lifecycle, schema, driver, registry);
    assert.equal(outcome.applied.length, 3, "replace/add/drop each applied once");
    assert.deepEqual(
      await pgPrimaryKeyColumns(client, schema, "replace_pk_items"),
      ["tenant_id", "external_id"],
    );
    assert.deepEqual(await pgPrimaryKeyColumns(client, schema, "add_pk_items"), ["a", "b"]);
    assert.deepEqual(await pgPrimaryKeyColumns(client, schema, "drop_pk_items"), []);
    assert.equal(
      await pgIdentityKind(client, schema, "replace_pk_items", "id"),
      null,
      "DROP IDENTITY physically removed generation from the old key",
    );
    const after = await client.query(
      `SELECT id, tenant_id, external_id, payload
         FROM ${pgIdent(schema)}.replace_pk_items ORDER BY id`,
    );
    assert.deepEqual(after.rows, before.rows, "key replacement preserves existing rows");
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

test("MySQL primary-key replace/add/drop changes live keys and removes AUTO_INCREMENT", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL PK-lifecycle e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("idlife_altpk_my");
  const meta = `${database}_migrations`;
  const base = primaryKeyBaseMigration();
  const lifecycle = primaryKeyLifecycleMigration();
  const registry = ownership("replace_pk_items", "add_pk_items", "drop_pk_items");
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(base, database, driver);
    // InnoDB normally advances AUTO_INCREMENT as soon as an explicit high value
    // is imported. This still exercises the shipped synchronization step and its
    // postcondition; the PostgreSQL arm above supplies the genuinely-behind
    // allocator transition, while this arm proves MySQL preserves the safe floor.
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.replace_pk_items
         (tenant_id, external_id, payload)
       VALUES (1, 101, 'first'), (1, 102, 'second');
       INSERT INTO ${mysqlIdent(database)}.add_pk_items (a, b, payload)
       VALUES (2, 201, 'add');
       INSERT INTO ${mysqlIdent(database)}.drop_pk_items (a, b, payload)
       VALUES (3, 301, 'drop')`,
    );
    const [beforeRows] = await admin.query(
      `SELECT id, tenant_id, external_id, payload
         FROM ${mysqlIdent(database)}.replace_pk_items ORDER BY id`,
    );

    const outcome = await applyAfter(base, lifecycle, database, driver, registry);
    assert.equal(outcome.applied.length, 3, "replace/add/drop each applied once");
    assert.deepEqual(
      await mysqlPrimaryKeyColumns(admin, database, "replace_pk_items"),
      ["tenant_id", "external_id"],
    );
    assert.deepEqual(
      await mysqlPrimaryKeyColumns(admin, database, "add_pk_items"),
      ["a", "b"],
    );
    assert.deepEqual(await mysqlPrimaryKeyColumns(admin, database, "drop_pk_items"), []);
    assert.equal(
      await mysqlIdentityKind(admin, database, "replace_pk_items", "id"),
      null,
      "AUTO_INCREMENT was physically removed from the old key",
    );
    const [afterRows] = await admin.query(
      `SELECT id, tenant_id, external_id, payload
         FROM ${mysqlIdent(database)}.replace_pk_items ORDER BY id`,
    );
    assert.deepEqual(afterRows, beforeRows, "key replacement preserves existing rows");
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

test("PostgreSQL synchronizeIdentity advances imported max and preserves an ahead generator", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL identity-sync e2e skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueSchema("idlife_sync_pg");
  const meta = `${schema}_migrations`;
  const base = synchronizeIdentityBaseMigration();
  const synchronize = synchronizeIdentityMigration();
  const registry = ownership("identity_behind", "identity_ahead");
  const driver = { kind: "postgres" as const, url: PG_URL };
  const importedMax = 500n;
  const aheadValue = 10_000n;

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await applyInitial(base, schema, driver);
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.identity_behind (id, payload) VALUES ($1, 'imported')`,
      [importedMax.toString()],
    );
    await client.query(
      `INSERT INTO ${pgIdent(schema)}.identity_ahead (id, payload) VALUES (50, 'imported')`,
    );
    await client.query(
      `SELECT pg_catalog.setval(
         pg_catalog.pg_get_serial_sequence(
           pg_catalog.format('%I.%I', $1::text, 'identity_ahead'), 'id'
         )::regclass,
         $2::bigint,
         true
       )`,
      [schema, aheadValue.toString()],
    );
    const aheadBefore = await pgIdentityState(client, schema, "identity_ahead", "id");
    assert.equal(aheadBefore.lastValue, aheadValue, "ahead setup moved the generator");

    const outcome = await applyAfter(base, synchronize, schema, driver, registry);
    assert.equal(outcome.applied.length, 2, "both synchronization steps are journaled");
    const behindState = await pgIdentityState(client, schema, "identity_behind", "id");
    const aheadAfter = await pgIdentityState(client, schema, "identity_ahead", "id");
    assert.equal(behindState.lastValue, importedMax, "behind generator synchronized to imported max");
    assert.deepEqual(aheadAfter, aheadBefore, "already-ahead generator is a monotonic no-op");

    const generatedBehind = await client.query(
      `INSERT INTO ${pgIdent(schema)}.identity_behind (payload)
       VALUES ('generated') RETURNING id::text`,
    );
    const generatedBehindId = BigInt(generatedBehind.rows[0].id);
    assert.equal(
      generatedBehindId,
      importedMax + behindState.increment,
      "next generated id respects the owned sequence increment",
    );
    const generatedAhead = await client.query(
      `INSERT INTO ${pgIdent(schema)}.identity_ahead (payload)
       VALUES ('generated') RETURNING id::text`,
    );
    assert.equal(
      BigInt(generatedAhead.rows[0].id),
      aheadValue + aheadAfter.increment,
      "ahead allocator was not moved backward",
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

test("MySQL synchronizeIdentity keeps explicit imports safe and preserves an ahead generator", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL identity-sync e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("idlife_sync_my");
  const meta = `${database}_migrations`;
  const base = synchronizeIdentityBaseMigration();
  const synchronize = synchronizeIdentityMigration();
  const registry = ownership("identity_behind", "identity_ahead");
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query("SET SESSION information_schema_stats_expiry = 0");
    const [variables] = await admin.query(
      `SELECT CAST(@@SESSION.auto_increment_increment AS CHAR) AS increment_by,
              CAST(@@SESSION.auto_increment_offset AS CHAR) AS offset_by`,
    );
    const variable = (variables as Array<Record<string, unknown>>)[0];
    const increment = BigInt(String(variable.increment_by ?? variable.INCREMENT_BY));
    const offset = BigInt(String(variable.offset_by ?? variable.OFFSET_BY));
    const importedMax = offset + increment * 500n;
    const aheadImport = offset + increment * 50n;
    const requestedAhead = offset + increment * 10_000n;

    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(base, database, driver);
    await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.identity_behind (id, payload) VALUES (?, 'imported');
       INSERT INTO ${mysqlIdent(database)}.identity_ahead (id, payload) VALUES (?, 'imported')`,
      [importedMax.toString(), aheadImport.toString()],
    );
    await admin.query(
      `ALTER TABLE ${mysqlIdent(database)}.identity_ahead AUTO_INCREMENT = ${requestedAhead}`,
    );
    const aheadBefore = await mysqlIdentityCounter(admin, database, "identity_ahead");

    const outcome = await applyAfter(base, synchronize, database, driver, registry);
    assert.equal(outcome.applied.length, 2, "both synchronization steps are journaled");
    const aheadAfter = await mysqlIdentityCounter(admin, database, "identity_ahead");
    assert.equal(aheadAfter, aheadBefore, "already-ahead AUTO_INCREMENT is a monotonic no-op");

    const [behindInsert] = await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.identity_behind (payload) VALUES ('generated')`,
    );
    const generatedBehind = BigInt(
      String((behindInsert as unknown as { insertId: number | string }).insertId),
    );
    assert.ok(generatedBehind > importedMax, "generated id is above the imported maximum");
    assert.equal(
      (generatedBehind - importedMax) % increment,
      0n,
      "generated id respects auto_increment_increment",
    );
    const [aheadInsert] = await admin.query(
      `INSERT INTO ${mysqlIdent(database)}.identity_ahead (payload) VALUES ('generated')`,
    );
    const generatedAhead = BigInt(
      String((aheadInsert as unknown as { insertId: number | string }).insertId),
    );
    assert.equal(generatedAhead, aheadBefore, "ahead allocator was not moved backward");
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

test("PostgreSQL clean ID facets survive host status and the catalog oracle reports tampering", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL ID-facet drift e2e skipped");
    return;
  }
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueSchema("idlife_drift_pg");
  const meta = `${schema}_migrations`;
  const migration = driftFacetMigration();
  const registry = ownership("drift_parents", "drift_children");
  const driver = { kind: "postgres" as const, url: PG_URL };

  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await applyInitial(migration, schema, driver);
    const clean = await pgIdFacetSnapshot(client, schema);
    assert.ok(clean.formatCheck, "TypeID format CHECK exists in the live catalog");
    assert.match(clean.formatCheck.definition, /CHECK/i);
    assert.match(clean.formatCheck.definition, /parent_/i, "CHECK retains the TypeID prefix");
    assert.equal(clean.identity, "by default", "identity facet is catalog-visible");
    assert.ok(clean.foreignKey, "typed foreign key exists in the live catalog");
    assert.deepEqual({
      localColumns: clean.foreignKey.localColumns,
      referencedTable: clean.foreignKey.referencedTable,
      referencedColumns: clean.foreignKey.referencedColumns,
      onDelete: clean.foreignKey.onDelete,
      onUpdate: clean.foreignKey.onUpdate,
      match: clean.foreignKey.match,
    }, {
      localColumns: ["parent_id"],
      referencedTable: "drift_parents",
      referencedColumns: ["id"],
      onDelete: "CASCADE",
      onUpdate: "CASCADE",
      match: "SIMPLE",
    });
    await assertCleanStatus(migration, schema, driver, registry);
    assert.deepEqual(diffIdFacets(clean, await pgIdFacetSnapshot(client, schema)), []);

    await client.query(
      `ALTER TABLE ${pgIdent(schema)}.drift_parents
         DROP CONSTRAINT ${pgIdent(clean.formatCheck.name)};
       ALTER TABLE ${pgIdent(schema)}.drift_children
         ALTER COLUMN id DROP IDENTITY;
       ALTER TABLE ${pgIdent(schema)}.drift_children
         DROP CONSTRAINT ${pgIdent(clean.foreignKey.name)}`,
    );
    const drift = diffIdFacets(clean, await pgIdFacetSnapshot(client, schema));
    assert.deepEqual(
      drift.map((entry) => entry.field).sort(),
      ["format", "identity", "reference"],
      `catalog drift report: ${JSON.stringify(drift)}`,
    );
    assert.ok(drift.every((entry) => entry.actual === null), "each dropped facet is reported");
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

test("MySQL clean ID facets survive host status and the catalog oracle reports tampering", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL ID-facet drift e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("idlife_drift_my");
  const meta = `${database}_migrations`;
  const migration = driftFacetMigration();
  const registry = ownership("drift_parents", "drift_children");
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(migration, database, driver);
    const clean = await mysqlIdFacetSnapshot(admin, database);
    assert.ok(clean.formatCheck, "TypeID format CHECK exists in the live catalog");
    assert.ok(clean.formatCheck.definition.length > 0, "format CHECK definition is recovered");
    assert.match(clean.formatCheck.definition, /parent_/i, "CHECK retains the TypeID prefix");
    assert.equal(clean.identity, "auto_increment", "identity facet is catalog-visible");
    assert.ok(clean.foreignKey, "typed foreign key exists in the live catalog");
    assert.deepEqual({
      localColumns: clean.foreignKey.localColumns,
      referencedTable: clean.foreignKey.referencedTable,
      referencedColumns: clean.foreignKey.referencedColumns,
      onDelete: clean.foreignKey.onDelete,
      onUpdate: clean.foreignKey.onUpdate,
    }, {
      localColumns: ["parent_id"],
      referencedTable: "drift_parents",
      referencedColumns: ["id"],
      onDelete: "CASCADE",
      onUpdate: "CASCADE",
    });
    await assertCleanStatus(migration, database, driver, registry);
    assert.deepEqual(diffIdFacets(clean, await mysqlIdFacetSnapshot(admin, database)), []);

    await admin.query(
      `ALTER TABLE ${mysqlIdent(database)}.drift_parents
         DROP CHECK ${mysqlIdent(clean.formatCheck.name)};
       ALTER TABLE ${mysqlIdent(database)}.drift_children
         MODIFY COLUMN id BIGINT NOT NULL;
       ALTER TABLE ${mysqlIdent(database)}.drift_children
         DROP FOREIGN KEY ${mysqlIdent(clean.foreignKey.name)}`,
    );
    const drift = diffIdFacets(clean, await mysqlIdFacetSnapshot(admin, database));
    assert.deepEqual(
      drift.map((entry) => entry.field).sort(),
      ["format", "identity", "reference"],
      `catalog drift report: ${JSON.stringify(drift)}`,
    );
    assert.ok(drift.every((entry) => entry.actual === null), "each dropped facet is reported");
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
