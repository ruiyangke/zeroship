// Standalone Node-native authoring -> IR -> apply test.
//
// Proves the V8-FREE authoring path end-to-end, entirely in the Node process:
//   1. the pure-JS host recorder (`host-recorder.ts`) evals the migration DSL
//      (`table()`/`t.*` from `zero-migrate`) into a `{ ir_version, name, ops }`
//      op-IR envelope — NO embedded V8, NO in-Rust recorder;
//   2. the `zero-migrate-node` napi addon LOWERs the envelope in Rust (stamps
//      `owner_app`, folds the authoritative `Checksum::of_ir` + the confined system
//      shape) and APPLIES it over the real `pg` npm driver via the `hostDriver`
//      seam — exactly the napi-bridge path.
//
// OFFLINE arm (always runs, DB-free): author the envelope and assert its shape
// (ir_version from the addon, op count, op kinds). This is the pure-JS recorder
// proof — no DB, no V8.
//
// FULL arm (gated by `connectLivePg`, see `live-db.ts`): apply into a fresh unique schema
// and assert the journal recorded the migration AND the `widgets` table + its author
// columns exist. This proves author -> IR -> lower(Rust checksum/fold) -> apply over
// the real `pg` driver.
//
// The addon `.node` is resolved via `ZERO_MIGRATE_ADDON_PATH` (set below to the
// sibling crate's build output) or the addon loader's dev-fallback.

import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { table, t } from "zero-migrate";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import {
  currentIrVersion,
  apply,
  history,
  plan,
  resolvePending,
  status,
} from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";
import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));

// Point the addon loader at the sibling crate's prebuilt `.node` unless the caller
// already set an explicit path. The napi default triple spelling on Linux is
// `<platform>-<arch>-gnu`.
if (!process.env.ZERO_MIGRATE_ADDON_PATH) {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  process.env.ZERO_MIGRATE_ADDON_PATH = join(
    HERE,
    `../../../../crates/zero-migrate-node/zero-migrate-node.${platform}-${arch}${abi}.node`,
  );
}

test("programmatic executor verbs require explicit policy bytes", async () => {
  const driver = {
    kind: "postgres" as const,
    url: "postgres://127.0.0.1:1/never_connect",
  };
  await assert.rejects(
    apply({
      migration: { up() {} },
      ownerApp: "app_policy_required",
      projectSchema: "policy_required",
      driver,
    } as never),
    /apply requires an explicit policy/,
  );
  await assert.rejects(
    status({
      ownerApp: "app_policy_required",
      projectSchema: "policy_required",
      driver,
      migrations: [{ up() {} }],
    } as never),
    /status requires an explicit policy/,
  );
  await assert.rejects(
    status({
      ownerApp: "app_policy_required",
      projectSchema: "policy_required",
      driver,
    } as never),
    /status requires an explicit policy/,
  );
  await assert.rejects(
    history({
      ownerApp: "app_policy_required",
      projectSchema: "policy_required",
      driver,
    } as never),
    /history requires an explicit policy/,
  );
  await assert.rejects(
    resolvePending({
      ownerApp: "app_policy_required",
      projectSchema: "policy_required",
      pendingVersion: "mig_0000000000000000000001",
      action: "apply",
      driver,
      approved: true,
    } as never),
    /resolvePending requires an explicit policy/,
  );
});

const PG_URL = pgUrl();

/** Import the sample migration (`.ts`) — resolves `zero-migrate` to this
 *  package's dist (one shared recorder singleton). Runs under `node --import tsx`. */
async function loadMigration() {
  return import("./mig/20260711000001_create_widgets.ts");
}

/** A fresh, unique schema so parallel runs / reruns never collide. */
function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

// ---------------------------------------------------------------------------
// OFFLINE arm — pure-JS recorder authors the op-IR envelope (no DB, no V8).
// ---------------------------------------------------------------------------
test("Node-native authoring: pure-JS recorder drains the DSL into an op-IR envelope", async () => {
  const mig = await loadMigration();
  const irVersion = currentIrVersion();
  assert.ok(irVersion > 0, "addon irVersion() must be a positive integer");

  const envelope = buildEnvelope(mig as never, {
    irVersion,
    nameFallback: "create_widgets",
  });

  assert.equal(envelope.ir_version, irVersion, "envelope ir_version comes from the addon");
  assert.equal(envelope.name, "create_widgets");
  assert.equal(envelope.ops.length, 2, "two authored ops (createTable + addColumn)");

  const kinds = (envelope.ops as Array<{ op: string }>).map((o) => o.op);
  assert.deepEqual(kinds, ["createTable", "addColumn"], "op kinds in declared order");

  // The recorder does NOT compute a checksum or set owner_app — those are Rust-owned.
  assert.ok(!("checksum" in envelope), "recorder must not fold a checksum");
  assert.ok(!("owner_app" in envelope), "recorder must not stamp owner_app");

  // The author columns are present PRE system-shape fold (the fold happens in the
  // addon's Rust lower, not in the JS recorder).
  const createTable = envelope.ops[0] as { columns: Array<{ name: string }> };
  const authored = new Set(createTable.columns.map((c) => c.name));
  for (const col of ["label", "status"]) {
    assert.ok(authored.has(col), `author column ${col} recorded on createTable`);
  }
});

test("plan authors once and validates the envelope it returns", () => {
  let calls = 0;
  const migration = {
    up() {
      calls += 1;
      table("single_pass").create({
        columns: {
          value: t.int(),
        },
      });
    },
  };

  const report = plan({
    migration,
    ownerApp: "app_single_pass",
    dialect: "postgres",
  });

  assert.equal(report.ok, true);
  assert.equal(calls, 1);
  assert.equal(report.envelope.ops.length, 1);
});

// ---------------------------------------------------------------------------
// FULL arm: napi addon lowers + applies over the real `pg` driver, gated by
// `connectLivePg`.
// ---------------------------------------------------------------------------
test("Node-native apply: napi addon lowers + applies the authored IR over the pg driver", async (t) => {
  const probe = await connectLivePg(t);
  if (!probe) return;

  const mig = await loadMigration();
  const schema = uniqueSchema("node_authoring");
  const meta = `${schema}_migrations`;

  try {
    // The apply pins ops into a pre-existing confined project schema.
    await probe.query(`CREATE SCHEMA "${schema}"`);

    const outcome = await apply({
      migration: mig as never,
      ownerApp: "app_widgets",
      projectSchema: schema,
      driver: { kind: "postgres", url: PG_URL },
      registry: {},
      policy: [noInjectPolicy(schema)],
      approved: false,
      appliedBy: "deploy",
      nameFallback: "create_widgets",
    });

    assert.ok(outcome.applied.length > 0, "at least one migration id applied");

    // The `widgets` table exists in the project schema.
    const tbl = await probe.query(
      `SELECT to_regclass('"${schema}".widgets') IS NOT NULL AS ex`,
    );
    assert.equal(tbl.rows[0].ex, true, "widgets table was created");

    // The explicit no-inject policy preserves the author-owned table shape.
    const cols = await probe.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'widgets'`,
      [schema],
    );
    const colNames = new Set(cols.rows.map((r: { column_name: string }) => r.column_name));
    for (const col of ["label", "status", "qty"]) {
      assert.ok(colNames.has(col), `author column ${col} present`);
    }
    for (const col of ["id", "created_at", "updated_at", "version"]) {
      assert.ok(!colNames.has(col), `no policy-managed system column ${col} is injected`);
    }

    // The journal recorded the applied migration steps.
    const journal = await probe.query(
      `SELECT name FROM "${meta}".schema_migrations WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    const journalNames = journal.rows.map((r: { name: string }) => r.name);
    assert.ok(journalNames.length > 0, "journal has applied rows");
    assert.ok(
      journalNames.includes("create_table_widgets"),
      `journal records the create_table step (got ${JSON.stringify(journalNames)})`,
    );
    assert.ok(
      journalNames.includes("add_column_widgets_qty"),
      `journal records the add_column step (got ${JSON.stringify(journalNames)})`,
    );
  } finally {
    await probe
      .query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${meta}" CASCADE`)
      .catch(() => {});
    await probe.end().catch(() => {});
  }
});

test("Node-native apply uses live PostgreSQL facts for existing-table changes", async (context) => {
  const probe = await connectLivePg(context);
  if (!probe) return;

  const schema = uniqueSchema("node_live_lower");
  const meta = `${schema}_migrations`;
  const ownerApp = "app_live_lower";
  const driver = { kind: "postgres" as const, url: PG_URL };
  try {
    await probe.query(`
      CREATE SCHEMA "${schema}";
      CREATE TABLE "${schema}".accounts (
        id bigint PRIMARY KEY,
        email text NOT NULL,
        status text
      );
      CREATE UNIQUE INDEX accounts_email_key ON "${schema}".accounts (email);
      CREATE TABLE "${schema}".rename_items (
        id bigint PRIMARY KEY,
        old_label text
      );
      INSERT INTO "${schema}".rename_items (id, old_label)
      VALUES (1, 'first'), (2, 'second')
    `);

    const dropUnique = {
      up() {
        table("accounts").index("accounts_email_key").drop();
      },
    };
    await assert.rejects(
      apply({
        migration: dropUnique,
        ownerApp,
        projectSchema: schema,
        driver,
        registry: { accounts: ownerApp, rename_items: ownerApp },
        policy: [noInjectPolicy(schema)],
        approved: false,
        appliedBy: "test",
        nameFallback: "drop_unique_index",
      }),
      /approval/i,
      "a live UNIQUE index drop must require approval even though JavaScript carries no unique hint",
    );
    const stillUnique = await probe.query(
      `SELECT to_regclass('"${schema}".accounts_email_key') IS NOT NULL AS ex`,
    );
    assert.equal(stillUnique.rows[0].ex, true, "refused apply keeps the unique index");

    await apply({
      migration: dropUnique,
      ownerApp,
      projectSchema: schema,
      driver,
      registry: { accounts: ownerApp, rename_items: ownerApp },
      policy: [noInjectPolicy(schema)],
      approved: true,
      appliedBy: "test",
      nameFallback: "drop_unique_index",
    });

    await apply({
      migration: {
        up() {
          table("accounts").column("status").setDefault("ready");
        },
      },
      ownerApp,
      projectSchema: schema,
      driver,
      registry: { accounts: ownerApp, rename_items: ownerApp },
      policy: [noInjectPolicy(schema)],
      approved: false,
      appliedBy: "test",
      nameFallback: "set_existing_default",
    });
    const defaultValue = await probe.query(
      `SELECT column_default FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'accounts' AND column_name = 'status'`,
      [schema],
    );
    assert.match(defaultValue.rows[0].column_default, /ready/);

    const renameMigration = {
      up() {
        table("rename_items")
          .column("old_label")
          .rename({ to: "label", type: t.text() });
      },
    };
    const renameOutcome = await apply({
      migration: renameMigration,
      ownerApp,
      projectSchema: schema,
      driver,
      registry: { accounts: ownerApp, rename_items: ownerApp },
      policy: [noInjectPolicy(schema)],
      approved: true,
      appliedBy: "test",
      nameFallback: "rename_existing_column",
    });
    assert.equal(renameOutcome.pendingContracts.length, 1);
    const pending = renameOutcome.pendingContracts[0];
    assert.equal(pending.table, "rename_items");
    assert.equal(pending.fromColumn, "old_label");
    assert.equal(pending.toColumn, "label");
    const renamed = await probe.query(
      `SELECT old_label, label FROM "${schema}".rename_items ORDER BY id`,
    );
    assert.deepEqual(renamed.rows, [
      { old_label: "first", label: "first" },
      { old_label: "second", label: "second" },
    ]);

    const resolution = {
      ownerApp,
      projectSchema: schema,
      pendingVersion: pending.pendingVersion,
      action: "apply" as const,
      driver,
      approved: true as const,
      policy: [noInjectPolicy(schema)],
      appliedBy: "test",
    };
    await probe.query(
      `CREATE VIEW "${schema}".rename_items_old_labels AS
       SELECT old_label FROM "${schema}".rename_items`,
    );
    await assert.rejects(
      resolvePending(resolution),
      /depend|cannot drop/i,
      "a source-column dependency must leave the contract pending",
    );
    const triggersAfterPartial = await probe.query(
      `SELECT count(*)::int AS n
         FROM pg_trigger trigger
         JOIN pg_class target ON target.oid = trigger.tgrelid
         JOIN pg_namespace namespace ON namespace.oid = target.relnamespace
        WHERE namespace.nspname = $1
          AND target.relname = 'rename_items'
          AND NOT trigger.tgisinternal`,
      [schema],
    );
    assert.equal(
      triggersAfterPartial.rows[0].n,
      1,
      "failed cleanup must roll back the trigger drop and column drop together",
    );
    await probe.query(`DROP VIEW "${schema}".rename_items_old_labels`);

    const resolved = await resolvePending(resolution);
    assert.equal(resolved.pendingContracts.length, 0);
    const completed = await probe.query(
      `SELECT label FROM "${schema}".rename_items ORDER BY id`,
    );
    assert.deepEqual(completed.rows, [{ label: "first" }, { label: "second" }]);
    const oldColumn = await probe.query(
      `SELECT EXISTS (
         SELECT 1 FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = 'rename_items' AND column_name = 'old_label'
       ) AS ex`,
      [schema],
    );
    assert.equal(oldColumn.rows[0].ex, false, "contract completion drops the old column");
    const settled = await status({
      ownerApp,
      projectSchema: schema,
      driver,
      registry: { rename_items: ownerApp },
      policy: [noInjectPolicy(schema)],
      migrations: [renameMigration],
      nameFallbacks: ["rename_existing_column"],
    });
    assert.equal(settled.pendingContracts.length, 0);
    assert.equal(settled.plans?.[0]?.state, "applied");
    const replayed = await apply({
      migration: renameMigration,
      ownerApp,
      projectSchema: schema,
      driver,
      registry: { accounts: ownerApp, rename_items: ownerApp },
      policy: [noInjectPolicy(schema)],
      approved: true,
      appliedBy: "test",
      nameFallback: "rename_existing_column",
    });
    assert.equal(
      replayed.pendingContracts.length,
      0,
      "a completed rename file must remain a terminal no-op on later deploys",
    );
  } finally {
    await probe
      .query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${meta}" CASCADE`)
      .catch(() => {});
    await probe.end().catch(() => {});
  }
});
