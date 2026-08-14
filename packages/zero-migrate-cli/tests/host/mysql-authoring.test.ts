// Live-MySQL host apply e2e — the MySQL analogue of
// `authoring.test.ts`, GATED behind `ZERO_MIGRATE_MYSQL_URL`.
//
// Proves the V8-FREE authoring path end-to-end against a REAL MySQL server, using
// the SAME `SqlSession` seam Postgres rides:
//   1. the pure-JS host recorder drains the migration DSL into a `{ ir_version,
//      name, ops }` op-IR envelope (no embedded V8, no in-Rust recorder);
//   2. the `zero-migrate-node` napi addon LOWERs the envelope in Rust (stamps
//      `owner_app`, folds the authoritative `Checksum::of_ir`, folds the confined
//      system shape) for the `mysql` dialect, then drives `executor::apply_with_lock_mysql`
//      — the `MysqlBackend` (`GET_LOCK`, MySQL journal DDL, `?` placeholders) — over
//      the real `mysql2` npm driver (`driver-mysql2.ts`) via the `hostDriver` seam.
//
// This is the structural proof that a `{ kind: "mysql" }` driver routes
// into the MySQL backend, NOT the Postgres executor, and applies a real migration.
//
// GATING: unless `ZERO_MIGRATE_MYSQL_URL` is set, the whole test SKIPS cleanly, so
// DB-free CI stays green. Set it to e.g.
//   mysql://root:root@127.0.0.1:3310/zmtest
// (a dedicated throwaway server).

import { test } from "node:test";
import assert from "node:assert/strict";

import { apply } from "zero-migrate-cli";
import { byteValue, decimal, ids, lit, table, t, uuidV4 } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";


// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

/** Import the shared sample migration (`createTable widgets` + `addColumn qty`). */
async function loadMigration() {
  return import("./mig/20260711000001_create_widgets.ts");
}

/** A fresh, unique database name so parallel runs / reruns never collide. MySQL
 *  identifiers cap at 64 chars and `<db>_migrations` must also fit, so keep it short. */
function uniqueDatabase(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

const VALID_BARE_TYPE_IDS = [
  "00000000000000000000000000",
  "00000000000000000000000001",
  "0000000000000000000000000a",
  "0000000000000000000000000g",
  "00000000000000000000000010",
  "7zzzzzzzzzzzzzzzzzzzzzzzzz",
] as const;

const VALID_PREFIXED_TYPE_IDS = [
  ["prefixed", "prefix_0123456789abcdefghjkmnpqrs"],
  ["prefixed", "prefix_01h455vb4pex5vsknk084sn02q"],
  ["split", "pre_fix_00000000000000000000000000"],
] as const;

const INVALID_OFFICIAL_TYPE_IDS = [
  "PREFIX_00000000000000000000000000",
  "12345_00000000000000000000000000",
  "pre.fix_00000000000000000000000000",
  "préfix_00000000000000000000000000",
  " prefix_00000000000000000000000000",
  "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl_00000000000000000000000000",
  "_00000000000000000000000000",
  "_",
  "prefix_1234567890123456789012345",
  "prefix_123456789012345678901234567",
  "prefix_1234567890123456789012345 ",
  "prefix_0123456789ABCDEFGHJKMNPQRS",
  "prefix_123456789-123456789-123456",
  "prefix_ooooooiiiiiiuuuuuuulllllll",
  "prefix_i23456789ol23456789oi23456",
  "prefix_123456789-0123456789-0123456",
  "prefix_8zzzzzzzzzzzzzzzzzzzzzzzzz",
  "_prefix_00000000000000000000000000",
  "prefix__00000000000000000000000000",
  "",
  "prefix_",
] as const;

const VALID_ULIDS = [
  "00000000000000000000000000",
  "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "7ZZZZZZZZZZZZZZZZZZZZZZZZZ",
] as const;

const INVALID_ULIDS = [
  "01ARZ3NDEKTSV4RRFFQ69G5FA",
  "01ARZ3NDEKTSV4RRFFQ69G5FAV0",
  "8ZZZZZZZZZZZZZZZZZZZZZZZZZ",
  "01arz3ndektsv4rrffq69g5fav",
  "01ARZ3NDEKTSV4RRFFQ69G5FAI",
  "01ARZ3NDEKTSV4RRFFQ69G5FAL",
  "01ARZ3NDEKTSV4RRFFQ69G5FAO",
  "01ARZ3NDEKTSV4RRFFQ69G5FAU",
  "01ARZ3NDEKTSV4RRFFQ69G5FA-",
] as const;

// ---------------------------------------------------------------------------
// Live-MySQL apply — the napi addon lowers for the `mysql` dialect + applies over
// the real `mysql2` driver into a fresh throwaway database. Skips (does not fail)
// when `ZERO_MIGRATE_MYSQL_URL` is unset.
// ---------------------------------------------------------------------------
test("Live MySQL apply: napi addon lowers + applies the authored IR over the mysql2 driver", async (t) => {
  if (!MYSQL_URL) {
    t.skip("ZERO_MIGRATE_MYSQL_URL unset — live-MySQL e2e skipped (DB-free CI stays green)");
    return;
  }

  // The `mysql2` driver is an optionalDependency; fail the test loudly if the URL
  // is set but the driver is missing (a real misconfiguration, not a skip).
  const mysql = (await import("mysql2/promise")).default;

  // A root/admin connection: creates the project database + reads back the journal
  // and schema. The apply itself opens its OWN pinned session via the host facade.
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });

  const database = uniqueDatabase("node_mysql");
  const meta = `${database}_migrations`;

  try {
    // MySQL qualifies the created table as `<database>`.`widgets`, so the project
    // database must exist before apply (the addon's journal bootstrap creates only
    // the `<database>_migrations` meta database). The e2e owns the project database.
    await admin.query(`CREATE DATABASE \`${database}\``);

    const mig = await loadMigration();

    const outcome = await apply({
      migration: mig as never,
      ownerApp: "app_widgets",
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "deploy",
      nameFallback: "create_widgets",
    });

    assert.ok(outcome.applied.length > 0, "at least one migration id applied");

    // The `widgets` table exists in the project database.
    const [tblRows] = await admin.query(
      `SELECT COUNT(*) AS n FROM information_schema.tables
        WHERE table_schema = ? AND table_name = 'widgets'`,
      [database],
    );
    assert.equal(
      Number((tblRows as Array<{ n: number | string }>)[0].n),
      1,
      "widgets table was created in the project database",
    );

    // The explicit no-inject policy preserves the author-owned table shape.
    const [colRows] = await admin.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = ? AND table_name = 'widgets'`,
      [database],
    );
    const colNames = new Set(
      (colRows as Array<{ column_name?: string; COLUMN_NAME?: string }>).map(
        (r) => (r.column_name ?? r.COLUMN_NAME) as string,
      ),
    );
    for (const col of ["label", "status", "qty"]) {
      assert.ok(colNames.has(col), `author column ${col} present (got ${JSON.stringify([...colNames])})`);
    }
    for (const col of ["id", "created_at", "updated_at", "version"]) {
      assert.ok(!colNames.has(col), `no policy-managed system column ${col} is injected`);
    }

    // The MySQL journal (in the `<database>_migrations` meta database) recorded the
    // applied migration steps. The MySQL journal is the single consolidated events
    // table, discriminated by `event_kind = 'applied'`.
    const [journalRows] = await admin.query(
      `SELECT name FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journalNames = (journalRows as Array<{ name?: string; NAME?: string }>).map(
      (r) => (r.name ?? r.NAME) as string,
    );
    assert.ok(journalNames.length > 0, "MySQL journal has applied rows");
    assert.ok(
      journalNames.includes("create_table_widgets"),
      `journal records the create_table step (got ${JSON.stringify(journalNames)})`,
    );
    assert.ok(
      journalNames.includes("add_column_widgets_qty"),
      `journal records the add_column step (got ${JSON.stringify(journalNames)})`,
    );

    // The journal is the MySQL native total order (BIGINT AUTO_INCREMENT event_seq).
    const [seqRows] = await admin.query(
      `SELECT MIN(event_seq) AS lo, MAX(event_seq) AS hi FROM \`${meta}\`.schema_migrations`,
    );
    const lo = Number((seqRows as Array<{ lo: number | string }>)[0].lo);
    const hi = Number((seqRows as Array<{ hi: number | string }>)[0].hi);
    assert.ok(hi >= lo && lo >= 1, "event_seq is a positive monotonic native total order");
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("Live MySQL TypeID CHECK enforces the official fixtures and empty-prefix form", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset — live-MySQL TypeID fixtures skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("mysql_type_id");
  const meta = `${database}_migrations`;
  const migration = {
    name: "mysql_type_id_fixtures",
    default: {
      schema() {
        table("type_id_samples").create({
          columns: {
            bare: ids.typeId({ prefix: "" }),
            prefixed: ids.typeId({ prefix: "prefix" }),
            split: ids.typeId({ prefix: "pre_fix" }),
          },
        });
      },
    },
  };

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await apply({
      migration,
      ownerApp: "app_mysql_type_id",
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "type-id-test",
    });

    for (const value of VALID_BARE_TYPE_IDS) {
      await admin.query(
        `INSERT INTO \`${database}\`.\`type_id_samples\` (bare) VALUES (?)`,
        [value],
      );
    }
    for (const [column, value] of VALID_PREFIXED_TYPE_IDS) {
      await admin.query(
        `INSERT INTO \`${database}\`.\`type_id_samples\` (\`${column}\`) VALUES (?)`,
        [value],
      );
    }
    await admin.query(`INSERT INTO \`${database}\`.\`type_id_samples\` () VALUES ()`);

    for (const value of INVALID_OFFICIAL_TYPE_IDS) {
      await assert.rejects(
        admin.query(
          `INSERT INTO \`${database}\`.\`type_id_samples\` (prefixed) VALUES (?)`,
          [value],
        ),
        `invalid official TypeID fixture was accepted: ${JSON.stringify(value)}`,
      );
    }
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("Live MySQL ULID CHECK enforces canonical uppercase spelling and bounds", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset — live-MySQL ULID fixtures skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("mysql_ulid");
  const meta = `${database}_migrations`;
  const migration = {
    name: "mysql_ulid_fixtures",
    default: {
      schema() {
        table("ulid_samples").create({
          columns: {
            id: ids.ulid(),
          },
        });
      },
    },
  };

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await apply({
      migration,
      ownerApp: "app_mysql_ulid",
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "ulid-test",
    });

    for (const value of VALID_ULIDS) {
      await admin.query(
        `INSERT INTO \`${database}\`.\`ulid_samples\` (id) VALUES (?)`,
        [value],
      );
    }
    await admin.query(
      `INSERT INTO \`${database}\`.\`ulid_samples\` (id) VALUES (NULL)`,
    );

    for (const value of INVALID_ULIDS) {
      await assert.rejects(
        admin.query(
          `INSERT INTO \`${database}\`.\`ulid_samples\` (id) VALUES (?)`,
          [value],
        ),
        `invalid ULID fixture was accepted: ${JSON.stringify(value)}`,
      );
    }
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("Live MySQL UUIDv4 default generates canonical RFC 9562 version and variant bits", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset — live-MySQL UUIDv4 test skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("mysql_uuid_v4");
  const meta = `${database}_migrations`;
  const migration = {
    name: "mysql_uuid_v4_default",
    default: {
      schema() {
        table("uuid_samples").create({
          columns: {
            id: t.uuid().notNull().default(uuidV4()),
          },
        });
      },
    },
  };

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);
    await apply({
      migration,
      ownerApp: "app_mysql_uuid_v4",
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "uuid-test",
    });

    for (let i = 0; i < 128; i += 1) {
      await admin.query(`INSERT INTO \`${database}\`.\`uuid_samples\` () VALUES ()`);
    }
    const [rows] = await admin.query(
      `SELECT id FROM \`${database}\`.\`uuid_samples\` ORDER BY id`,
    );
    const values = (rows as Array<{ id?: string; ID?: string }>).map(
      (row) => (row.id ?? row.ID) as string,
    );
    assert.equal(values.length, 128);
    for (const value of values) {
      assert.match(
        value,
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
        `exact lowercase RFC 9562 UUIDv4: ${value}`,
      );
      assert.equal(value[14], "4", `version bits are 0100: ${value}`);
      assert.ok("89ab".includes(value[19]), `variant bits are 10: ${value}`);
    }
    // Every format assertion above passes for a generator that returns one
    // constant well-formed v4 UUID 128 times, so none of them proves the column
    // default generates anything. This one does: 128 rows must hold 128 distinct
    // values. It catches a constant or near-constant generator and nothing more
    // - 128 samples cannot detect bias, low entropy, or a long repeat period.
    assert.equal(
      new Set(values).size,
      values.length,
      "128 UUIDv4 column defaults are all distinct",
    );
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("Live MySQL onConflict updates only the authored target and journals only committed steps", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; live MySQL onConflict test skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("mysql_conflict");
  const meta = `${database}_migrations`;
  const schemaMigration = {
    name: "mysql_conflict_schema",
    default: {
      schema() {
        const items = table("conflict_items");
        items.create({
          columns: {
            id: t.int().primaryKey(),
            code: t.char({ length: 32 }).notNull().unique(),
            nullable_key: t.char({ length: 32 }).unique(),
            label: t.text().notNull(),
            amount: t.numeric({ precision: 30, scale: 10 }).notNull(),
            payload: t.bytes().notNull(),
          },
        });
      },
    },
  };
  const dataMigration = {
    name: "mysql_conflict_target",
    default: {
      data() {
        const items = table("conflict_items");
        items.insert({
          rows: {
            id: 1,
            code: "original",
            nullable_key: null,
            label: "seed",
            amount: decimal("1.0000000000"),
            payload: byteValue(new Uint8Array([1])),
          },
        });
        items.insert({
          rows: {
            id: 1,
            code: "different",
            label: "incoming",
            amount: decimal("2.0000000000"),
            payload: byteValue(new Uint8Array([2])),
          },
          onConflict: {
            columns: ["id"],
            doUpdate: {
              label: "target-updated",
              amount: decimal("12345678901234567890.1234567890"),
              payload: byteValue(new Uint8Array([0, 0x80, 0xff])),
            },
          },
        });
        items.insert({
          rows: {
            id: 2,
            code: "original",
            nullable_key: null,
            label: "wrong-row",
            amount: decimal("3.0000000000"),
            payload: byteValue(new Uint8Array([3])),
          },
          onConflict: {
            columns: ["nullable_key"],
            doUpdate: { label: "must-not-apply" },
          },
        });
      },
      inverse() {
        table("conflict_items").delete({ where: (col) => col("id").in([1, 2]) });
      },
    },
  };

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);

    const deploy = (migration: typeof schemaMigration | typeof dataMigration) =>
      apply({
        migration: migration as never,
        ownerApp: "app_mysql_conflict",
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: { conflict_items: "app_mysql_conflict" },
        policy: [noInjectPolicy(database)],
        approved: false,
        appliedBy: "on-conflict-test",
        nameFallback: migration.name,
      });

    await deploy(schemaMigration);

    await assert.rejects(
      deploy(dataMigration),
      /Invalid JSON|JSON_EXTRACT|3141/i,
      "NULL values on a nullable UNIQUE target must not match a different-key collision",
    );

    const [amountColumns] = await admin.query(
      `SELECT column_type FROM information_schema.columns
        WHERE table_schema = ? AND table_name = 'conflict_items' AND column_name = 'amount'`,
      [database],
    );
    const amountColumn = (amountColumns as Array<Record<string, unknown>>)[0] ?? {};
    assert.equal(
      String(amountColumn.column_type ?? amountColumn.COLUMN_TYPE),
      "decimal(30,10)",
      "t.numeric({ precision, scale }) creates a fixed-precision MySQL column",
    );

    const [rows] = await admin.query(
      `SELECT id, code, label, CAST(amount AS CHAR) AS amount, HEX(payload) AS payload
         FROM \`${database}\`.conflict_items ORDER BY id`,
    );
    const visible = (rows as Array<Record<string, unknown>>).map((row) => ({
      id: Number(row.id ?? row.ID),
      code: String(row.code ?? row.CODE),
      label: String(row.label ?? row.LABEL),
      amount: String(row.amount ?? row.AMOUNT),
      payload: String(row.payload ?? row.PAYLOAD),
    }));
    assert.deepEqual(visible, [
      {
        id: 1,
        code: "original",
        label: "target-updated",
        amount: "12345678901234567890.1234567890",
        payload: "0080FF",
      },
    ]);

    const [journal] = await admin.query(
      `SELECT name FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journalNames = (journal as Array<{ name: string }>).map((row) => row.name);
    assert.equal(
      journalNames.filter((name) => name === "insert conflict_items").length,
      2,
      "the failed non-target conflict has no applied journal event",
    );
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

test("Live MySQL onConflict rejects a non-unique authored target before mutation", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; live MySQL onConflict target proof skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueDatabase("mysql_conflict_nonunique");
  const meta = `${database}_migrations`;
  const schemaMigration = {
    name: "mysql_conflict_nonunique_schema",
    default: {
      schema() {
        const items = table("nonunique_items");
        items.create({
          columns: {
            id: t.int().primaryKey(),
            group_id: t.int().notNull(),
            label: t.text().notNull(),
          },
        });
      },
    },
  };
  const dataMigration = {
    name: "mysql_conflict_nonunique_target",
    default: {
      data() {
        const items = table("nonunique_items");
        items.insert({ rows: { id: 1, group_id: 7, label: "seed" } });
        items.insert({
          rows: { id: 1, group_id: 7, label: "incoming" },
          onConflict: {
            columns: ["group_id"],
            doUpdate: { label: "must-not-apply" },
          },
        });
      },
      inverse() {
        table("nonunique_items").delete({ where: (col) => col("id").eq(1) });
      },
    },
  };

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);

    const deploy = (migration: typeof schemaMigration | typeof dataMigration) =>
      apply({
        migration: migration as never,
        ownerApp: "app_mysql_conflict_nonunique",
        projectSchema: database,
        driver: { kind: "mysql", url: MYSQL_URL },
        registry: { nonunique_items: "app_mysql_conflict_nonunique" },
        policy: [noInjectPolicy(database)],
        approved: false,
        appliedBy: "on-conflict-target-proof",
        nameFallback: migration.name,
      });

    await deploy(schemaMigration);

    await assert.rejects(
      deploy(dataMigration),
      /does not exactly match the full columns of one UNIQUE or PRIMARY index/,
    );

    const [rows] = await admin.query(
      `SELECT id, group_id, label FROM \`${database}\`.nonunique_items ORDER BY id`,
    );
    assert.deepEqual(
      (rows as Array<Record<string, unknown>>).map((row) => ({
        id: Number(row.id ?? row.ID),
        group_id: Number(row.group_id ?? row.GROUP_ID),
        label: String(row.label ?? row.LABEL),
      })),
      [{ id: 1, group_id: 7, label: "seed" }],
      "the committed seed remains when the later non-unique target is rejected",
    );

    const [journal] = await admin.query(
      `SELECT name FROM \`${meta}\`.schema_migrations
        WHERE event_kind = 'applied' AND name = 'insert nonunique_items'`,
    );
    assert.equal(
      (journal as Array<unknown>).length,
      1,
      "only the seed insert is journaled; the unproven target step never runs",
    );
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

// An expression column default authored through the public DSL and applied to a
// real MySQL server.
//
// `lower` is one of the scalar functions a default is allowed to call, so this
// shape is reachable from user-facing authoring rather than from a hand-built IR.
// MySQL requires a non-literal DEFAULT to be parenthesised: `DEFAULT lower('X')`
// is a syntax error, `DEFAULT (lower('X'))` is accepted. Measured against the
// container this suite runs on (8.4.11), by issuing the statements directly:
//
//     ADD COLUMN `label` VARCHAR(64) ... DEFAULT lower(_utf8mb4 X'58')
//       ERROR 1064 (42000): You have an error in your SQL syntax
//     ADD COLUMN `label` VARCHAR(64) ... DEFAULT 'plain'
//       accepted
//
// The literal migration below is the CONTROL and it applies first. It differs
// from the expression migration in exactly one variable - the default - so a
// failure on the second apply while the first succeeds attributes the failure to
// the default and not to the harness, the database, or the authoring shape.
test("Live MySQL applies an expression column default, with a literal default as the control", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset - live-MySQL expression-default test skipped");
    return;
  }

  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
  });
  const database = uniqueDatabase("mysql_expr_default");
  const meta = `${database}_migrations`;
  const ownerApp = "app_mysql_expr_default";

  const literalDefault = {
    name: "mysql_literal_default_control",
    default: {
      schema() {
        table("literal_labels").create({
          columns: {
            label: t.string({ length: 64 }).notNull().default("plain"),
          },
        });
      },
    },
  };
  const expressionDefault = {
    name: "mysql_expression_default",
    default: {
      schema() {
        table("expr_labels").create({
          columns: {
            label: t
              .string({ length: 64 })
              .notNull()
              .default(lit("X").lower()),
          },
        });
      },
    },
  };

  const deploy = (migration: { name: string }) =>
    apply({
      migration,
      ownerApp,
      projectSchema: database,
      driver: { kind: "mysql", url: MYSQL_URL },
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: false,
      appliedBy: "expression-default-test",
      nameFallback: migration.name,
    });

  try {
    await admin.query(`CREATE DATABASE \`${database}\``);

    await deploy(literalDefault);
    await admin.query(`INSERT INTO \`${database}\`.\`literal_labels\` () VALUES ()`);
    const [controlRows] = await admin.query(
      `SELECT label FROM \`${database}\`.\`literal_labels\``,
    );
    assert.deepEqual(
      (controlRows as Array<Record<string, unknown>>).map((row) =>
        String(row.label ?? row.LABEL),
      ),
      ["plain"],
      "the control applies and its literal default populates, so the harness reaches MySQL",
    );

    await deploy(expressionDefault);
    await admin.query(`INSERT INTO \`${database}\`.\`expr_labels\` () VALUES ()`);
    const [rows] = await admin.query(`SELECT label FROM \`${database}\`.\`expr_labels\``);
    assert.deepEqual(
      (rows as Array<Record<string, unknown>>).map((row) => String(row.label ?? row.LABEL)),
      ["x"],
      "the expression default evaluates on insert rather than storing the unevaluated text",
    );

    // MySQL records a parenthesised default as an expression and reports its own
    // normalised spelling, so assert on the flag and the function it names rather
    // than on an exact string this server is free to reformat.
    const [catalog] = await admin.query(
      `SELECT COLUMN_DEFAULT AS d, EXTRA AS e
         FROM information_schema.COLUMNS
        WHERE TABLE_SCHEMA = '${database}' AND TABLE_NAME = 'expr_labels'
          AND COLUMN_NAME = 'label'`,
    );
    const column = (catalog as Array<{ d: string | null; e: string | null }>)[0];
    assert.ok(column, "the applied column is in the catalog");
    assert.equal(
      column.e,
      "DEFAULT_GENERATED",
      "MySQL classes the applied default as an expression, not a literal",
    );
    assert.match(
      String(column.d ?? ""),
      /lower\(/i,
      `the stored default is the authored function: ${column.d}`,
    );
  } finally {
    await admin
      .query(`DROP DATABASE IF EXISTS \`${database}\`; DROP DATABASE IF EXISTS \`${meta}\``)
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
