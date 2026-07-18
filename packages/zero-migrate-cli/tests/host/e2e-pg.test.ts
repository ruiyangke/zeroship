// Cross-language e2e parity oracle over the REAL addon + real `pg` driver vs live PG.
//
// The SAME multi-op golden migration (`mig/20260712000001_create_gadgets.ts`:
// createTable + addColumn + createIndex) runs THROUGH the shipped host path — the
// pure-JS recorder → the napi addon's Rust lower (owner_app stamp + Checksum::of_ir
// fold) → `executor::apply` over the real `pg` npm
// driver seam — and we assert the applied SCHEMA and the JOURNAL match the engine's
// expectation. This is the Node-side peer of the in-crate live-PG regression suite
// both drive the identical shipped PostgresBackend-over-seam apply, one
// via the dev-only Rust `SqlSession`, one via the production napi/`pg` bridge.
//
// The typed napi verbs exercised: `apply` (ApplyReply),
// `status` (StatusReply), `history` (HistoryReply) — no JSON-string plumbing.
//
// Coverage (oracles, adapted to the shipped seam):
//   - multi-op apply: createTable + addColumn + createIndex all land (schema oracle);
//   - journal rows + ordering: every step is journaled, in strict `event_seq` order,
//     under one shared `Checksum::of_ir` anchor;
//   - status()/history() over the host driver reconcile to the applied set;
//   - drift/checksum: two applies of the identical artifact fold the SAME anchor
//     (drift-free identity), a modified artifact folds a DIFFERENT anchor (drift).
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL` (the same var the in-crate suite + `authoring.test`
// use). Auto-skips cleanly when the test Postgres is unreachable, so DB-free CI stays
// green. Runs under `node --import tsx --test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { apply, status, history, currentIrVersion } from "zero-migrate-cli";
import { table, t } from "zero-migrate";
import { NO_INJECT_POLICY_CEILING } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));

// Point the addon loader at the sibling crate's prebuilt `.node` unless the caller
// already set an explicit path (the napi default Linux triple is `<plat>-<arch>-gnu`).
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
const OWNER_APP = "app_gadgets";
const DRIVER = { kind: "postgres" as const, url: PG_URL };

/** A fresh, unique schema so parallel runs / reruns never collide. */
function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Import the multi-op golden migration (`.ts`) — resolves `zero-migrate` to this
 *  package's dist (one shared recorder singleton). Runs under `node --import tsx`. */
async function loadMigration() {
  return import("./mig/20260712000001_create_gadgets.ts");
}

/** Connect a `pg.Client`, or `null` if the test Postgres is unreachable (→ skip). */
async function tryConnect(): Promise<import("pg").Client | null> {
  const pg = (await import("pg")).default;
  const c = new pg.Client({ connectionString: PG_URL });
  try {
    await c.connect();
    return c;
  } catch {
    await c.end().catch(() => {});
    return null;
  }
}

/** Read the `applied` journal rows (event_seq, name, checksum), in `event_seq` order,
 *  from the `<schema>_migrations` META schema. */
async function readJournal(
  client: import("pg").Client,
  projectSchema: string,
): Promise<Array<{ event_seq: string; name: string; checksum: string }>> {
  const meta = `${projectSchema}_migrations`;
  const r = await client.query(
    `SELECT event_seq, name, checksum
       FROM "${meta}".schema_migrations
      WHERE event_kind = 'applied'
      ORDER BY event_seq`,
  );
  return r.rows as never;
}

/** Apply a migration module into a fresh schema, return the distinct journal checksum
 *  anchor(s). Cleans up the project + meta schema. */
async function applyAndAnchors(
  client: import("pg").Client,
  migration: unknown,
  schema: string,
): Promise<string[]> {
  const meta = `${schema}_migrations`;
  await client.query(`CREATE SCHEMA "${schema}"`);
  try {
    await apply({
      migration: migration as never,
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver: DRIVER,
      registry: {},
      policyCeiling: NO_INJECT_POLICY_CEILING,
      appliedBy: "deploy",
      nameFallback: "create_gadgets",
    });
    const r = await client.query(
      `SELECT DISTINCT checksum FROM "${meta}".schema_migrations WHERE event_kind = 'applied'`,
    );
    return (r.rows as Array<{ checksum: string }>).map((row) => row.checksum);
  } finally {
    await client
      .query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${meta}" CASCADE`)
      .catch(() => {});
  }
}

// ---------------------------------------------------------------------------
// The full e2e oracle: multi-op apply + journal + status/history + drift/checksum,
// all through the REAL addon + real `pg` driver. Auto-skips if PG is unreachable.
// ---------------------------------------------------------------------------
test("e2e-pg: multi-op apply + journal + status/history + drift, real addon + pg driver", async (tc) => {
  const client = await tryConnect();
  if (!client) {
    tc.skip(`test Postgres unreachable at ${PG_URL} (set ZERO_MIGRATE_TEST_PG_URL)`);
    return;
  }

  const mig = await loadMigration();
  const schema = uniqueSchema("e2e_gadgets");
  const meta = `${schema}_migrations`;

  try {
    // ---- Multi-op apply through the real napi/pg host path -----------------
    await client.query(`CREATE SCHEMA "${schema}"`);
    const outcome = await apply({
      migration: mig as never,
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver: DRIVER,
      registry: {},
      policyCeiling: NO_INJECT_POLICY_CEILING,
      approved: false,
      appliedBy: "deploy",
      nameFallback: "create_gadgets",
    });
    assert.ok(outcome.applied.length > 0, "at least one migration id applied");
    assert.equal(outcome.skipped.length, 0, "nothing skipped on a fresh schema");
    assert.equal(outcome.recovered.length, 0, "no non-txn recovery on a clean apply");
    // The applied version ids are unique mig_… ids.
    assert.equal(
      new Set(outcome.applied).size,
      outcome.applied.length,
      "applied version ids are distinct",
    );

    // ---- Schema oracle: all three op kinds physically landed ---------------
    // 1. createTable: the `gadgets` table exists with only its authored columns.
    const cols = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'gadgets'`,
      [schema],
    );
    const colNames = new Set(cols.rows.map((r: { column_name: string }) => r.column_name));
    for (const c of ["sku", "kind"]) {
      assert.ok(colNames.has(c), `createTable author column ${c} present`);
    }
    // 2. addColumn — the ALTER-added `price` column is present.
    assert.ok(colNames.has("price"), "addColumn `price` present (ALTER add landed)");
    for (const c of ["id", "created_at", "updated_at", "version"]) {
      assert.ok(!colNames.has(c), `no policy-managed system column ${c} is injected`);
    }
    // 3. createIndex — the authored `gadgets_sku_idx` index physically exists.
    const idx = await client.query(
      `SELECT indexname FROM pg_indexes WHERE schemaname = $1 AND tablename = 'gadgets'`,
      [schema],
    );
    const idxNames = new Set(idx.rows.map((r: { indexname: string }) => r.indexname));
    assert.ok(idxNames.has("gadgets_sku_idx"), "createIndex `gadgets_sku_idx` landed");

    // ---- Journal oracle: rows + strict event_seq ordering + one anchor -----
    const journal = await readJournal(client, schema);
    assert.ok(journal.length > 0, "journal has applied rows");
    // Every applied version id is journaled.
    assert.ok(
      journal.length >= outcome.applied.length,
      `journal (${journal.length}) covers every applied id (${outcome.applied.length})`,
    );
    // The author steps are journaled by their engine step names, in declared order.
    const stepNames = journal.map((r) => r.name);
    const ctIdx = stepNames.indexOf("create_table_gadgets");
    const acIdx = stepNames.indexOf("add_column_gadgets_price");
    const ciIdx = stepNames.indexOf("create_index_gadgets_sku_idx");
    assert.ok(ctIdx >= 0, `journal records create_table step (got ${JSON.stringify(stepNames)})`);
    assert.ok(acIdx >= 0, `journal records add_column step (got ${JSON.stringify(stepNames)})`);
    assert.ok(ciIdx >= 0, `journal records create_index step (got ${JSON.stringify(stepNames)})`);
    // Declared authoring order is preserved: createTable < addColumn < createIndex.
    assert.ok(ctIdx < acIdx && acIdx < ciIdx, "author steps journaled in declared order");
    // event_seq is a strictly increasing exact int8 sequence (connection-scoped exact
    // integer parsers → no float rounding; positional monotonicity).
    const seqs = journal.map((r) => {
      const s = String(r.event_seq);
      assert.ok(/^\d+$/.test(s), `event_seq ${s} is an exact integer string`);
      return BigInt(s);
    });
    for (let i = 1; i < seqs.length; i++) {
      assert.ok(seqs[i] > seqs[i - 1], "event_seq strictly increasing");
    }
    // One shared Checksum::of_ir anchor across every step of the one artifact.
    const anchors = new Set(journal.map((r) => r.checksum));
    assert.equal(anchors.size, 1, `one checksum anchor across all steps (got ${anchors.size})`);
    const anchor = [...anchors][0];
    assert.ok(/^[0-9a-f]{64}$/.test(anchor), "checksum anchor is a 64-hex digest");

    // ---- status()/history() typed verbs over the host driver ---------------
    const st = await status({
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver: DRIVER,
      policyCeiling: NO_INJECT_POLICY_CEILING,
      migrations: [mig as never],
      nameFallbacks: ["create_gadgets"],
    });
    // Plan-aware status reports one logical plan while retaining the actual
    // journal identities as its ordered steps.
    assert.equal(st.plans?.length, 1, "one supplied plan reconciled");
    assert.equal(st.plans?.[0]?.state, "applied", "complete plan is applied");
    assert.deepEqual(
      (st.plans?.[0]?.steps.map((step) => step.version) ?? []).sort(),
      [...outcome.applied].sort(),
      "status plan steps == apply outcome",
    );
    assert.deepEqual(st.applied, [st.plans?.[0]?.version], "logical plan is applied");
    assert.equal(st.pending.length, 0, "status.pending empty");
    assert.equal(st.rolledBack.length, 0, "status.rolledBack empty");
    assert.equal(st.currentVersion, st.plans?.[0]?.version, "currentVersion is the plan id");

    const hist = await history({ ownerApp: OWNER_APP, projectSchema: schema, driver: DRIVER });
    assert.equal(hist.events.length, journal.length, "history has one event per journal row");
    // Every history event is `applied`, its `eventSeq` a real bigint, monotonic, and
    // carries the same anchor checksum.
    let prev = -1n;
    for (const ev of hist.events) {
      assert.equal(ev.kind, "applied", "history event kind is `applied`");
      assert.equal(typeof ev.eventSeq, "bigint", "eventSeq crosses as a bigint (napi6)");
      assert.ok(ev.eventSeq > prev, "history eventSeq strictly increasing");
      prev = ev.eventSeq;
      assert.equal(ev.checksum, anchor, "history event carries the shared anchor");
      assert.equal(ev.appliedBy, "deploy", "history event records appliedBy");
    }

    // ---- drift/checksum oracle --------------------------------------------
    // (a) Re-authoring + re-applying the IDENTICAL artifact into a fresh schema folds
    //     the SAME anchor — the drift-free cross-apply identity (Checksum::of_ir is a
    //     dialect-neutral function of the op list, not of the per-apply-minted version).
    const anchorsAgain = await applyAndAnchors(
      client,
      mig,
      uniqueSchema("e2e_gadgets_again"),
    );
    assert.equal(anchorsAgain.length, 1, "re-apply also folds a single anchor");
    assert.equal(anchorsAgain[0], anchor, "re-applying the same artifact folds the SAME anchor");

    // (b) A MODIFIED artifact (an extra column) folds a DIFFERENT anchor — drift is
    //     detected structurally in the checksum, not just at DDL time.
    const modified = {
      name: "create_gadgets",
      up() {
        table("gadgets").create({
          columns: {
            sku: t.text().notNull(),
            kind: t.text().notNull().default("widget"),
            note: t.text(),
          },
        });
        table("gadgets").column("price").add({ type: t.int() });
        table("gadgets").index("gadgets_sku_idx").add({ on: ["sku"] });
      },
    };
    const anchorsModified = await applyAndAnchors(
      client,
      modified,
      uniqueSchema("e2e_gadgets_drift"),
    );
    assert.notEqual(
      anchorsModified[0],
      anchor,
      "a modified artifact folds a DIFFERENT checksum anchor (drift detected)",
    );

    // ---- ir_version is the single source of truth (addon-owned) ------------
    assert.ok(currentIrVersion() > 0, "addon irVersion() is a positive integer");
  } finally {
    await client
      .query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE; DROP SCHEMA IF EXISTS "${meta}" CASCADE`)
      .catch(() => {});
    await client.end().catch(() => {});
  }
});
