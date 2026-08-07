// Poison-parser regression suite for the host PG driver's connection-scoped types.
//
// `pg.types.setTypeParser` is GLOBAL and MUTABLE: any module in the host process
// can rewrite the decoder for an OID and every later `pg` query in the process
// inherits it. `driver-pg.ts` answers that with a connection-scoped `types` object
// whose `getTypeParser` shadows the OIDs whose decode the seam depends on. These
// tests poison the global parsers BEFORE opening a session and assert the pinned
// decode still wins, mirroring the shape of the oid-20 poison in `tests/host/oracle.ts`.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL` (the same var the rest of the host suite uses).
// Auto-skips cleanly when the test Postgres is unreachable, so DB-free CI stays green.

import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import type { JsCell, JsReply, JsRequest } from "../../src/addon.js";
import { openPgSession, type HostDriver } from "../../src/driver-pg.js";
import { apply } from "zero-migrate-cli";
import { NO_INJECT_POLICY } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));

// Point the addon loader at the sibling crate's prebuilt `.node` unless the caller
// already set an explicit path (napi's Linux triple spelling is `<plat>-<arch>-gnu`).
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

/** A fresh, unique schema so parallel runs / reruns never collide. */
function uniqueSchema(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Probe the test Postgres; returns the reachability failure message or null. */
async function pgUnreachable(): Promise<string | null> {
  const pg = (await import("pg")).default;
  const probe = new pg.Client({ connectionString: PG_URL });
  try {
    await probe.connect();
    await probe.end();
    return null;
  } catch (e) {
    await probe.end().catch(() => {});
    return (e as Error).message;
  }
}

/** Drive one verb through the `hostDriver([request, done])` callback contract. */
function runVerb(driver: HostDriver, request: JsRequest): Promise<JsReply> {
  return new Promise((resolve, reject) => {
    driver([
      request,
      (err, reply) => {
        if (err) reject(new Error(err.message));
        else resolve(reply as JsReply);
      },
    ]);
  });
}

function query(sql: string): JsRequest {
  return { kind: "query", sql, binds: [], textParams: [] };
}

/**
 * Overwrite the GLOBAL text parsers for `oids` and return a restore function.
 * `poison` stands in for a host app (or a transitive dependency) that called
 * `pg.types.setTypeParser` for its own reasons: the raw wire string flows through
 * undecoded, which is the worst case for any decode the seam infers from the OID.
 */
async function poisonGlobalParsers(oids: number[]): Promise<() => void> {
  const pg = (await import("pg")).default;
  const saved = oids.map((oid) => [oid, pg.types.getTypeParser(oid)] as const);
  for (const oid of oids) pg.types.setTypeParser(oid, (v: string) => v);
  return () => {
    for (const [oid, parser] of saved) pg.types.setTypeParser(oid, parser as never);
  };
}

// ---------------------------------------------------------------------------
// oid 16 (bool): a poisoned global bool parser must not flip `false` to `true`.
//
// `valueToCell` classifies bool by OID (`oid === OID_BOOL`) and coerces with
// `Boolean(value)`. Under a poisoned global the value arrives as the raw string
// `"f"`, and `Boolean("f") === true`: a silent truth-flip with no error. The
// connection-scoped shadow for oid 16 is what keeps `false` false.
// ---------------------------------------------------------------------------
test("poisoned global bool parser cannot flip false to true (oid 16 pinned)", async (t) => {
  const unreachable = await pgUnreachable();
  if (unreachable !== null) {
    t.skip(`test Postgres unreachable at ${PG_URL}: ${unreachable}`);
    return;
  }

  const restore = await poisonGlobalParsers([16]);
  let session: Awaited<ReturnType<typeof openPgSession>> | null = null;
  try {
    session = await openPgSession(PG_URL);
    const reply = await runVerb(
      session.hostDriver,
      query("SELECT false AS f, true AS tr, NULL::bool AS n"),
    );

    assert.equal(reply.rows.length, 1);
    const cells = reply.rows[0].cells as JsCell[];
    assert.deepEqual(
      cells,
      [
        { kind: "bool", bool: false },
        { kind: "bool", bool: true },
        { kind: "null" },
      ],
      "the connection-scoped oid-16 parser must win over the poisoned global",
    );
  } finally {
    await session?.close().catch(() => {});
    restore();
  }
});

// ---------------------------------------------------------------------------
// The rest of the OIDs a host is plausibly tempted to override. int4 re-coerces
// with `Number(value)` and the apply path `::text`-casts `"char"`, so these are
// expected to survive a poisoned global unchanged; the assertion is that a full
// apply still produces the same catalog facts and the same exact journal.
// ---------------------------------------------------------------------------
test("apply survives poisoned global parsers for bool/char/int4/int8/text[]", async (t) => {
  const unreachable = await pgUnreachable();
  if (unreachable !== null) {
    t.skip(`test Postgres unreachable at ${PG_URL}: ${unreachable}`);
    return;
  }

  const pg = (await import("pg")).default;
  //   16 = bool, 18 = "char", 20 = int8, 23 = int4, 1009 = text[]
  const restore = await poisonGlobalParsers([16, 18, 20, 23, 1009]);

  const schema = uniqueSchema("poison_apply");
  const meta = `${schema}_migrations`;
  const adm = new pg.Client({ connectionString: PG_URL });
  await adm.connect();

  try {
    await adm.query(`CREATE SCHEMA "${schema}"`);

    const mig = await import("./mig/20260711000001_create_widgets.ts");
    const outcome = await apply({
      migration: mig as never,
      ownerApp: "app_widgets",
      projectSchema: schema,
      driver: { kind: "postgres", url: PG_URL },
      registry: {},
      policy: [NO_INJECT_POLICY],
      approved: false,
      appliedBy: "deploy",
      nameFallback: "create_widgets",
    });
    assert.ok(outcome.applied.length > 0, "at least one migration id applied");

    // Catalog facts: the authored table and its author columns landed.
    const cols = await adm.query(
      `SELECT column_name FROM information_schema.columns
       WHERE table_schema = $1 AND table_name = 'widgets'
       AND column_name IN ('label','status','qty')
       ORDER BY column_name`,
      [schema],
    );
    assert.deepEqual(
      cols.rows.map((r) => r.column_name),
      ["label", "qty", "status"],
      "the authored columns exist despite the poisoned globals",
    );

    // The journal's int8 `event_seq` stayed exact (the oid-20 pin) and every step
    // shares the one `Checksum::of_ir` anchor.
    const journal = await adm.query(
      `SELECT version, checksum, event_seq::text AS event_seq
       FROM "${meta}".schema_migrations
       WHERE event_kind = 'applied' ORDER BY event_seq`,
    );
    assert.ok(journal.rows.length > 0, "the apply journaled at least one step");
    for (const row of journal.rows) {
      assert.match(String(row.event_seq), /^\d+$/, "event_seq is an exact integer string");
    }
    assert.equal(
      new Set(journal.rows.map((r) => r.checksum)).size,
      1,
      "one Checksum::of_ir anchor across all steps",
    );
  } finally {
    await adm.query(`DROP SCHEMA IF EXISTS "${schema}" CASCADE`).catch(() => {});
    await adm.query(`DROP SCHEMA IF EXISTS "${meta}" CASCADE`).catch(() => {});
    await adm.end().catch(() => {});
    restore();
  }
});
