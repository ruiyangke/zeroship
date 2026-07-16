// Host PG driver — a thin `pg` wrapper the addon drives over its
// `hostDriver([request, done]) => void` TSFN contract.
//
// ONE pinned `pg.Client` per session (the addon pins one connection and is strictly
// one-verb-at-a-time). A verb request `{ kind, sql, binds, textParams }`
// becomes a `pg` query; the reply is `{ rows, rowCount }` on success or
// `{ error: { sqlstate, message } }` on failure — surfaced to the addon as
// `done(err, null)` / `done(null, reply)`.
//
// TWO integrity contracts this driver ENFORCES (not assumes):
//
//  1. int8/numeric/int8[] cross as STRINGS via CONNECTION-SCOPED type parsers. The
//     IR's exact-integer domain (`event_seq`, `version`, seq bounds) crosses as
//     `driver::Value::Text` and must never lose precision. node-pg's DEFAULT parsers
//     already return oid 20 (int8) / 1700 (numeric) as strings, BUT
//     `pg.types.setTypeParser` is GLOBAL and MUTABLE — a host app that overrode the
//     int8 parser to `Number` would silently truncate large bigints below the seam
//     with NO error. So we construct the Client with its OWN `types` object whose
//     `getTypeParser` forces oid 20/1700/1016 → `String`, independent of any global
//     override. The poison oracle proves these win.
//
//  2. `executeTextParams` is a DISTINCT path: it receives a
//     `(string | null)[]` and calls `client.query(sql, values)` with NO explicit
//     param type OIDs — `pg` sends them text-format and PG INFERS the target type
//     (e.g. a text literal → timestamptz). `null` → PG NULL; every
//     non-null crosses as its exact string, no coercion.

// `pg` is an optionalDependency. Imported lazily so a host that only uses
// SQLite (native rusqlite) never needs it installed.
type PgModule = typeof import("pg");
type PgClient = import("pg").Client;

// The neutral cell DTOs come from the GENERATED addon `index.d.ts` (via `addon.ts`)
// — the single source of truth. No hand-copied interfaces.
import type { JsCell, JsRow, JsRequest, JsReply, JsError } from "./addon.js";

/** The addon's host-driver callback contract: `hostDriver([request, done]) => void`
 *  — napi delivers `(request, done)` as a SINGLE array arg. */
export type HostDriver = (
  args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void],
) => void;

// OIDs whose values must cross as exact strings (the exact-integer domain).
const OID_INT8 = 20;
const OID_NUMERIC = 1700;
const OID_INT8_ARRAY = 1016;

/**
 * Build a connection-scoped `types` object whose `getTypeParser(oid, format)`
 * forces the exact-integer OIDs (int8/numeric/int8[]) to `String`, deferring every
 * other OID to node-pg's default parser. This is IMMUNE to a global
 * `pg.types.setTypeParser` override, because the Client is constructed with
 * this object as its own `types`.
 */
function connectionScopedTypes(pg: PgModule): { getTypeParser: (oid: number, format?: unknown) => (value: string) => unknown } {
  // The default parser factory: node-pg's own `types.getTypeParser`. We call it for
  // any OID we don't override, so int4/text/bool/etc. behave exactly as usual —
  // EXCEPT that even the default for oid 20/1700 is a string parser, so overriding
  // to `String` is belt-and-suspenders against a poisoned global default.
  const defaults = pg.types;
  const identityString = (value: string): string => value;
  return {
    getTypeParser(oid: number, format?: unknown): (value: string) => unknown {
      if (oid === OID_INT8 || oid === OID_NUMERIC) {
        // Exact integer / decimal: cross as the verbatim string. NEVER Number(x)
        // (which truncates > 2^53). This wins over any global override.
        return identityString;
      }
      if (oid === OID_INT8_ARRAY) {
        // int8[]: the ARRAY parser composed over a string element parser, so each
        // element stays exact. Delegate the array framing to node-pg's default
        // array parser but force the element parser to identity-string.
        const arrayParser = defaults.getTypeParser(OID_INT8_ARRAY as never, format as never);
        // node-pg's array parser already yields string elements when the element
        // parser is a string parser; the default int8[] parser stringifies elements
        // (it composes over the int8 string parser). Return it verbatim — it is
        // string-preserving by construction and immune to the scalar int8 override
        // because array parsing is registered separately.
        return arrayParser as (value: string) => unknown;
      }
      return defaults.getTypeParser(oid, format as never) as (value: string) => unknown;
    },
  };
}

/**
 * Open a pinned host PG session and return the `hostDriver` callback the addon
 * drives, plus a `close()` to release the connection. The Client is constructed
 * with connection-scoped exact-integer parsers. ONE Client per session; the
 * addon guarantees one verb at a time.
 */
export async function openPgSession(
  connectionString: string,
): Promise<{ hostDriver: HostDriver; client: PgClient; close: () => Promise<void> }> {
  const pg = (await import("pg")).default as unknown as PgModule;
  const client = new pg.Client({
    connectionString,
    // Connection-scoped parsers — immune to a global setTypeParser override.
    types: connectionScopedTypes(pg) as never,
  });
  await client.connect();

  const hostDriver: HostDriver = ([request, done]) => {
    runVerb(client, request).then(
      (reply) => done(null, reply),
      (err: unknown) => done(toJsError(err), null),
    );
  };

  return {
    hostDriver,
    client,
    close: async () => {
      await client.end();
    },
  };
}

/** Run one verb against the pinned Client, returning the neutral reply. */
async function runVerb(client: PgClient, request: JsRequest): Promise<JsReply> {
  switch (request.kind) {
    case "batch": {
      // A multi-statement DDL batch (no params, no returned rows the engine reads).
      await client.query(request.sql);
      return { rows: [], rowCount: undefined };
    }
    case "execute": {
      const result = await client.query(request.sql, cellsToParams(request.binds));
      return { rows: [], rowCount: result.rowCount ?? undefined };
    }
    case "executeTextParams": {
      // DISTINCT path: text-format params, NO explicit OID → PG infers
      // the target type. `null` → PG NULL; non-null crosses as its exact string.
      const values = request.textParams.map((v) => (v === null || v === undefined ? null : v));
      const result = await client.query(request.sql, values);
      return { rows: [], rowCount: result.rowCount ?? undefined };
    }
    case "query":
    case "queryOne": {
      const result = await client.query({
        text: request.sql,
        values: cellsToParams(request.binds),
        // Preserve column order + duplicate names positionally (the engine reads by
        // position via the parallel columns/cells vectors).
        rowMode: "array",
      });
      const columns = result.fields.map((f) => f.name);
      // Per-column OIDs: used to classify int8/numeric (→ exact `intStr` cell, the
      // seam's `driver::Value::Int` exact-integer domain) vs a genuine text column
      // (→ `text` cell). Without the OID we could not tell a stringified int8 from a
      // real text value.
      const oids = result.fields.map((f) => f.dataTypeID);
      const rows: JsRow[] = (result.rows as unknown[][]).map((arr) => ({
        columns,
        cells: arr.map((v, i) => valueToCell(v, oids[i])),
      }));
      return { rows, rowCount: result.rowCount ?? undefined };
    }
    default: {
      throw new Error(`host pg driver: unknown verb kind ${JSON.stringify(request.kind)}`);
    }
  }
}

/** Marshal neutral binds → `pg` param values (param side). */
function cellsToParams(binds: JsCell[]): unknown[] {
  return binds.map((cell) => {
    switch (cell.kind) {
      case "null":
        return null;
      case "text":
        return cell.text ?? null;
      case "int":
        // An exact integer bind crosses as `intStr` (int8) or `int` (int4). Send the
        // string form when present so a bigint bind stays exact; `pg` binds it
        // text-format and PG coerces to the column type.
        return cell.intStr ?? cell.int ?? null;
      case "bool":
        return cell.bool ?? null;
      case "textArray":
        return cell.textArray ?? null;
      default:
        return null;
    }
  });
}

// The int4 OID: crosses as a JS number → `int` cell (the seam's small catalog-int
// domain — `character_maximum_length`, row counts).
const OID_INT4 = 23;
const OID_INT2 = 21;
const OID_BOOL = 16;

/**
 * Marshal a `pg` result value (already parsed by the connection-scoped parsers) →
 * a neutral cell the addon deserializes to `driver::Value`, classified by the column's
 * OID:
 * - int8 (20) / numeric (1700): arrive as EXACT STRINGS via the scoped parser →
 *   `{ kind:"int", intStr }` (the seam's exact-integer `driver::Value::Int` domain —
 *   `event_seq`, `version`; NEVER `Number(x)`, which truncates > 2^53).
 * - int4 (23) / int2 (21): JS number → `{ kind:"int", int }`.
 * - bool (16): `{ kind:"bool" }`.
 * - a `string[]`: `{ kind:"textArray" }`.
 * - everything else (text/name/varchar/timestamp-as-text/…): `{ kind:"text" }`.
 * - `null`: `{ kind:"null" }`.
 */
function valueToCell(value: unknown, oid: number): JsCell {
  if (value === null || value === undefined) return { kind: "null" };
  if (typeof value === "boolean" || oid === OID_BOOL) {
    return { kind: "bool", bool: Boolean(value) };
  }
  if (Array.isArray(value)) {
    // text[] / int8[] (elements already stringified by the scoped array parser).
    return {
      kind: "textArray",
      textArray: value.map((el) => (el === null || el === undefined ? null : String(el))),
    };
  }
  if (oid === OID_INT8 || oid === OID_NUMERIC) {
    // Exact-integer domain: crossed as a STRING by the scoped parser → `intStr`.
    // This is the load-bearing contract for journal `event_seq`/`version`.
    return { kind: "int", intStr: String(value) };
  }
  if (oid === OID_INT4 || oid === OID_INT2) {
    // Small catalog int → a JS number cell.
    return { kind: "int", int: typeof value === "number" ? value : Number(value) };
  }
  if (typeof value === "bigint") {
    return { kind: "int", intStr: value.toString() };
  }
  if (typeof value === "number") {
    return { kind: "int", int: value };
  }
  if (typeof value === "string") {
    return { kind: "text", text: value };
  }
  // Fallback: JSON-stringify any structured value (jsonb, etc.) as text. A safety net.
  return { kind: "text", text: JSON.stringify(value) };
}

/** Marshal a thrown `pg` error → the neutral `JsError` (message + optional
 *  SQLSTATE), so `role.rs`'s message-only transient-retry classifier still fires
 *  and the engine surfaces the real DB error. */
function toJsError(err: unknown): JsError {
  if (err && typeof err === "object") {
    const e = err as { message?: unknown; code?: unknown };
    return {
      message: typeof e.message === "string" ? e.message : String(err),
      code: typeof e.code === "string" ? e.code : undefined,
    };
  }
  return { message: String(err) };
}
