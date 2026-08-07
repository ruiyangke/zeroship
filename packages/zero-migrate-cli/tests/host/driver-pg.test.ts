// Poison-parser regression suite for the host PG driver's connection-scoped types.
//
// `pg.types.setTypeParser` is GLOBAL and MUTABLE: any module in the host process
// can rewrite the decoder for an OID and every later `pg` query in the process
// inherits it. `driver-pg.ts` answers that with a connection-scoped `types` object
// whose `getTypeParser` pins the OIDs whose decode the seam depends on.
//
// These tests assert a PROPERTY, not a list of oids: no parser the driver relies on
// is reachable through `pg.types`. Every poison here is unconditional over all oids,
// because an enumerated poison is only ever as wide as its list, and a shadow that
// BORROWS its parser from `pg.types` passes an enumerated poison while pinning
// nothing (it only moves which oid has to be poisoned). Widening such a list later
// also READS as widening the check when it is not.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL` (the same var the rest of the host suite uses).
// The property arms need no database; the live arms auto-skip cleanly when the test
// Postgres is unreachable, so DB-free CI stays green.

import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import type { JsCell, JsReply, JsRequest } from "../../src/addon.js";
import {
  openPgSession,
  connectionScopedTypes,
  PINNED_OIDS,
  type HostDriver,
} from "../../src/driver-pg.js";
import { apply } from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";

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
 * Poison EVERY oid in the global `pg.types` map and return a restore function.
 *
 * `setTypeParser(oid, fn)` writes into the very map `getTypeParser(oid)` reads, so
 * replacing the read with ONE unconditional branch puts the map in exactly the state
 * a host app would leave it in after poisoning every oid. It is written as one branch
 * on purpose: a list of oids reads as the thing being asserted, and then the
 * assertion is only ever as wide as the list, which is how a borrowed parser passes
 * for a pin. The stand-in leaks the raw wire string, the worst case for any decode
 * the seam infers from a column OID.
 */
async function poisonEveryGlobalParser(): Promise<() => void> {
  const pg = (await import("pg")).default;
  const types = pg.types as unknown as {
    getTypeParser: unknown;
    setTypeParser: unknown;
  };
  const savedGet = types.getTypeParser;
  const savedSet = types.setTypeParser;
  const leakRawWireText = (value: string): string => value;
  types.getTypeParser = () => leakRawWireText;
  // A poisoned map cannot be un-poisoned from inside the process either.
  types.setTypeParser = () => {};
  return () => {
    types.getTypeParser = savedGet;
    types.setTypeParser = savedSet;
  };
}

// ---------------------------------------------------------------------------
// The pin PROPERTY, with no database: no parser `driver-pg` relies on is reachable
// through `pg.types`.
//
// The stand-in below answers EVERY oid, unconditionally, with a parser that throws,
// and records which oids were asked for. The driver's scoped `getTypeParser` must
// neither ask for nor call any of them for an oid it claims to pin. A shadow that
// borrows `defaults.getTypeParser(...)` fails both halves, which is the whole point:
// borrowing does not pin, it only moves which oid has to be poisoned.
// ---------------------------------------------------------------------------

interface PoisonedPg {
  types: {
    getTypeParser: (oid: number, format?: unknown) => (value: string) => unknown;
    setTypeParser: () => void;
  };
}

function poisonEveryOid(): { pg: PoisonedPg; reached: number[] } {
  const reached: number[] = [];
  return {
    reached,
    pg: {
      types: {
        getTypeParser(oid: number) {
          reached.push(oid);
          return () => {
            throw new Error(`pg.types parser for oid ${oid} was reached`);
          };
        },
        setTypeParser() {},
      },
    },
  };
}

/** Parse one wire value through the scoped types built over a fully poisoned `pg`. */
function scopedParse(oid: number, wire: string): unknown {
  const { pg } = poisonEveryOid();
  return connectionScopedTypes(pg as never).getTypeParser(oid)(wire);
}

// One wire sample per pinned oid, spelled exactly as PostgreSQL's `*_out` writes it.
// The test below asserts the key set EQUALS `PINNED_OIDS`, so pinning a new oid in
// the driver fails this suite until the sample is supplied: the driver owns the list,
// the test cannot drift behind it.
const WIRE_SAMPLE: Record<number, string> = {
  16: "f",
  20: "9223372036854775807",
  1003: "{id,created_at}",
  1009: "{a,b}",
  1016: "{9223372036854775807}",
  1700: "0.10",
};

const ascending = (a: number, b: number): number => a - b;

test("no parser the pg driver pins is reachable through pg.types", () => {
  const { pg, reached } = poisonEveryOid();
  const scoped = connectionScopedTypes(pg as never);

  assert.deepEqual(
    Object.keys(WIRE_SAMPLE).map(Number).sort(ascending),
    [...PINNED_OIDS].sort(ascending),
    "every oid the driver pins needs a wire sample in WIRE_SAMPLE",
  );

  for (const oid of PINNED_OIDS) {
    // Throws if the parser for this oid came from the poisoned map.
    scoped.getTypeParser(oid)(WIRE_SAMPLE[oid]);
  }

  assert.deepEqual(
    reached,
    [],
    "a pinned oid must be decoded by the driver itself, never looked up in pg.types",
  );
});

test("the poison stand-in is wired in: an unpinned oid still delegates to pg.types", () => {
  const UNPINNED_INT4 = 23;
  assert.ok(
    !PINNED_OIDS.includes(UNPINNED_INT4),
    "oid 23 must stay unpinned for this control to mean anything",
  );

  const { pg, reached } = poisonEveryOid();
  connectionScopedTypes(pg as never).getTypeParser(UNPINNED_INT4);

  assert.deepEqual(
    reached,
    [UNPINNED_INT4],
    "the scoped types must still defer unpinned oids to node-pg's defaults",
  );
});

test("the pinned array parser decodes the PostgreSQL array literal itself", () => {
  assert.deepEqual(scopedParse(1009, "{}"), [], "empty array");
  assert.deepEqual(scopedParse(1009, "{a,b}"), ["a", "b"], "bare elements");
  assert.deepEqual(scopedParse(1009, "{NULL}"), [null], "an unquoted NULL is a null element");
  assert.deepEqual(
    scopedParse(1009, '{"NULL"}'),
    ["NULL"],
    "a quoted NULL is the four-character string",
  );
  assert.deepEqual(scopedParse(1009, '{"",x}'), ["", "x"], "an empty element is quoted");
  assert.deepEqual(
    scopedParse(1009, '{"a,b","c\\"d","e\\\\f"}'),
    ["a,b", 'c"d', "e\\f"],
    "a quoted element keeps its comma, its escaped quote and its escaped backslash",
  );
  assert.deepEqual(
    scopedParse(1003, "{id,created_at}"),
    ["id", "created_at"],
    "name[] shares text[]'s array-literal syntax",
  );
});

test("int8[] elements cross as exact strings, never Number", () => {
  const parsed = scopedParse(1016, "{9223372036854775807,-9223372036854775808,NULL}");
  assert.deepEqual(parsed, ["9223372036854775807", "-9223372036854775808", null]);
  for (const element of parsed as unknown[]) {
    if (element !== null) assert.equal(typeof element, "string");
  }
});

test("an array literal the seam cannot represent is a loud error, not a truncation", () => {
  assert.throws(() => scopedParse(1009, "{{a,b},{c,d}}"), /nested/i, "nested braces");
  assert.throws(() => scopedParse(1009, "[0:1]={a,b}"), /array literal/i, "dimension prefix");
  assert.throws(() => scopedParse(1009, "{a,b"), /array literal/i, "unterminated array");
  assert.throws(() => scopedParse(1009, '{"a}'), /array literal/i, "unterminated quote");
});

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

  const restore = await poisonEveryGlobalParser();
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
// The array OIDs, against a live server. Under a fully poisoned global map the
// borrowed shadows hand back the raw `{a,b}` wire text, which `valueToCell`
// classifies as a `text` cell and the seam's `Vec<String>` (TextArray) decode then
// rejects. The pinned parser must produce the array itself.
// ---------------------------------------------------------------------------
test("poisoned global array parsers cannot collapse text[]/name[]/int8[] to raw text", async (t) => {
  const unreachable = await pgUnreachable();
  if (unreachable !== null) {
    t.skip(`test Postgres unreachable at ${PG_URL}: ${unreachable}`);
    return;
  }

  const restore = await poisonEveryGlobalParser();
  let session: Awaited<ReturnType<typeof openPgSession>> | null = null;
  try {
    session = await openPgSession(PG_URL);
    const reply = await runVerb(
      session.hostDriver,
      query(
        `SELECT ARRAY['a','b']::text[] AS t,
                ARRAY['id','created_at']::name[] AS n,
                ARRAY[9223372036854775807, -9223372036854775808]::int8[] AS i8,
                ARRAY['x', NULL, 'NULL', 'a,b', 'q"z']::text[] AS mixed,
                ARRAY[]::text[] AS empty`,
      ),
    );

    assert.equal(reply.rows.length, 1);
    assert.deepEqual(reply.rows[0].cells as JsCell[], [
      { kind: "textArray", textArray: ["a", "b"] },
      { kind: "textArray", textArray: ["id", "created_at"] },
      { kind: "textArray", textArray: ["9223372036854775807", "-9223372036854775808"] },
      { kind: "textArray", textArray: ["x", null, "NULL", "a,b", 'q"z'] },
      { kind: "textArray", textArray: [] },
    ]);
  } finally {
    await session?.close().catch(() => {});
    restore();
  }
});

// ---------------------------------------------------------------------------
// A full apply against a fully poisoned global map: the catalog facts and the
// exact journal are unchanged, so nothing the apply path reads depends on a
// parser a host app can rewrite.
// ---------------------------------------------------------------------------
test("apply survives a fully poisoned global pg.types map", async (t) => {
  const unreachable = await pgUnreachable();
  if (unreachable !== null) {
    t.skip(`test Postgres unreachable at ${PG_URL}: ${unreachable}`);
    return;
  }

  const pg = (await import("pg")).default;
  const restore = await poisonEveryGlobalParser();

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
      policy: [noInjectPolicy(schema)],
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
