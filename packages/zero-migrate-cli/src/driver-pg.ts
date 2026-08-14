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
//     `getTypeParser` forces oid 20/1700 to the verbatim string and decodes oid
//     16/1003/1009/1016 itself, consulting `pg.types` for NONE of them. The poison
//     oracle proves these win.
//
//     Oid 16 (bool) is pinned by the same mechanism for a different failure mode:
//     `valueToCell` classifies a bool by its column OID and coerces with
//     `Boolean(value)`, so a global override that leaks the raw wire string makes
//     `Boolean("f")` return `true` and turns every `false` into `true` with no error.
//     Bool is the only non-integer type that crosses uncast (the apply path
//     `::text`-casts enums, `"char"` and timestamps, and the int arms re-coerce with
//     `Number(value)`, which yields a loud NaN for garbage).
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
import { assertHostAllowed } from "./net-allowlist.js";

/** The addon's host-driver callback contract: `hostDriver([request, done]) => void`
 *  — napi delivers `(request, done)` as a SINGLE array arg. */
export type HostDriver = (
  args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void],
) => void;

/** Host-enforced transport controls (the host owns the socket). Mirrors
 *  `MysqlSessionOptions`; all optional. Structurally compatible with
 *  `NetworkSecurityOptions` in `index.ts`, which is what `openSession` passes. */
export interface PgSessionOptions {
  /** A PEM CA bundle to pin (TLS). When set, `pg` verifies the server cert. */
  tlsCa?: string;
  /** Reject the connection if the URL host is not in this allowlist (checked
   *  before the socket opens). */
  hostAllowlist?: string[];
  /** Per-verb query timeout in ms (applied by `pg` as `query_timeout`). */
  queryTimeoutMs?: number;
}

// OIDs whose values must cross as exact strings (the exact-integer domain).
const OID_INT8 = 20;
const OID_NUMERIC = 1700;
const OID_INT8_ARRAY = 1016;
// `name[]` (array_agg(attname)/array_agg(rolname) in catalog introspection) and
// `text[]`. node-pg parses `text[]` into a JS array by default but NOT `name[]`,
// which would otherwise arrive as raw `{a,b}` text and fail the seam's
// `Vec<String>` (TextArray) decode. Both are decoded by `parsePgArrayLiteral`, not by
// anything reachable through `pg.types`. See `connectionScopedTypes`.
const OID_NAME_ARRAY = 1003;
const OID_TEXT_ARRAY = 1009;
// `bool`. Pinned like the exact-integer OIDs because `valueToCell` classifies a bool
// by OID, so a poisoned global parser silently flips `false` to `true`. See
// `connectionScopedTypes`.
const OID_BOOL = 16;

/**
 * Every OID whose decode `connectionScopedTypes` pins. Exported so the poison suite
 * asserts the PROPERTY over the whole set (no parser this driver relies on is
 * reachable through `pg.types.setTypeParser`) instead of keeping its own hand-written
 * list of OIDs, which silently goes stale the moment a pin is added here.
 */
export const PINNED_OIDS: readonly number[] = [
  OID_BOOL,
  OID_INT8,
  OID_NUMERIC,
  OID_INT8_ARRAY,
  OID_NAME_ARRAY,
  OID_TEXT_ARRAY,
];

/**
 * Decode a text-format `bool` from the wire. Throws on anything other than the two
 * values PostgreSQL's `boolout` emits, so an unexpected encoding fails the verb
 * loudly instead of being guessed into a `true`.
 */
function parseBoolStrict(value: string): boolean {
  if (value === "t") return true;
  if (value === "f") return false;
  throw new Error(
    `host pg driver: unexpected bool wire value ${JSON.stringify(value)} (expected "t" or "f")`,
  );
}

/**
 * Decode a text-format PostgreSQL array literal into `(string | null)[]`.
 *
 * This parser MUST NOT consult `pg.types`: not for the array framing and not for the
 * element decode. `pg.types` is the global, mutable map `connectionScopedTypes` exists
 * to be immune from, so borrowing a parser out of it (even one registered under a
 * different OID) is not a pin: it only moves which OID a host app or a transitive
 * dependency has to override to poison the decode.
 *
 * Elements stay VERBATIM STRINGS. That is the load-bearing property for `int8[]`,
 * where `Number` would truncate above 2^53, and it is what the seam's `Vec<String>`
 * (TextArray) decode expects for `text[]`/`name[]`.
 *
 * Accepts exactly what PostgreSQL's `array_out` emits for a one-dimensional array:
 * `{}` for empty, `,`-separated elements, each either bare or double-quoted with
 * backslash escapes. A bare `NULL` is the null element; a quoted `"NULL"` is the
 * four-character string. Whitespace is never trimmed, because `array_out` never emits
 * any that is not part of a value.
 *
 * Anything else - a nested array, an explicit `[0:1]=` dimension prefix, a truncated
 * literal - THROWS. The seam's TextArray is one-dimensional, so a nested literal has
 * no faithful representation, and failing the verb is the only alternative to
 * silently flattening or truncating one.
 */
function parsePgArrayLiteral(wire: string): (string | null)[] {
  const reject = (why: string): never => {
    throw new Error(
      `host pg driver: unsupported PostgreSQL array literal (${why}): ${JSON.stringify(wire)}`,
    );
  };

  // A `[lo:hi]=` dimension prefix fails the leading-brace check, as intended.
  if (wire.charAt(0) !== "{") reject("must start with {");
  const close = wire.length - 1;
  if (close < 1 || wire.charAt(close) !== "}") reject("must end with }");

  const elements: (string | null)[] = [];
  let at = 1;
  if (at === close) return elements; // `{}`

  for (;;) {
    let element: string | null;
    const ch = wire.charAt(at);
    if (ch === "{") {
      reject("nested arrays are not supported by the one-dimensional TextArray seam");
    }
    if (ch === '"') {
      at += 1;
      let text = "";
      for (;;) {
        if (at >= close) reject("unterminated quoted element");
        const cur = wire.charAt(at);
        if (cur === "\\") {
          if (at + 1 >= close) reject("unterminated backslash escape");
          text += wire.charAt(at + 1);
          at += 2;
          continue;
        }
        if (cur === '"') {
          at += 1;
          break;
        }
        text += cur;
        at += 1;
      }
      element = text;
    } else {
      const from = at;
      while (at < close && wire.charAt(at) !== ",") {
        const cur = wire.charAt(at);
        if (cur === "{") {
          reject("nested arrays are not supported by the one-dimensional TextArray seam");
        }
        if (cur === "}" || cur === '"') reject("unexpected character inside a bare element");
        at += 1;
      }
      const bare = wire.slice(from, at);
      // Bare `NULL` is the null element; the quoted branch above never lands here, so
      // `"NULL"` keeps its four characters.
      element = bare === "NULL" ? null : bare;
    }

    elements.push(element);
    if (at === close) return elements;
    if (wire.charAt(at) !== ",") reject("expected , or } after an element");
    at += 1;
    if (at === close) reject("trailing element separator");
  }
}

/**
 * Build a connection-scoped `types` object whose `getTypeParser(oid, format)` forces
 * the exact-integer OIDs (int8/numeric) to the verbatim string and decodes bool and
 * the array OIDs itself, deferring every other OID to node-pg's default parser. This
 * is IMMUNE to a global `pg.types.setTypeParser` override, because the Client is
 * constructed with this object as its own `types` AND because no parser it returns
 * for a pinned OID is obtained from `pg.types`. A pin that BORROWS its parser from
 * `pg.types` is not a pin: it only moves which OID has to be poisoned. `PINNED_OIDS`
 * is the exported set of OIDs this rule covers.
 */
export function connectionScopedTypes(pg: PgModule): { getTypeParser: (oid: number, format?: unknown) => (value: string) => unknown } {
  // The default parser factory: node-pg's own `types.getTypeParser`. We call it for
  // any OID we don't override, so int4/text/etc. behave exactly as usual,
  // EXCEPT that even the default for oid 20/1700 is a string parser, so overriding
  // to `String` is belt-and-suspenders against a poisoned global default.
  const defaults = pg.types;
  const identityString = (value: string): string => value;
  return {
    getTypeParser(oid: number, format?: unknown): (value: string) => unknown {
      if (oid === OID_BOOL) {
        // bool: decode the wire text here rather than trusting the global parser.
        // `valueToCell` classifies a bool by OID and coerces with `Boolean(value)`,
        // so a global override that leaks the raw string turns `"f"` into `true`:
        // every `false` becomes `true` with no error to catch it.
        //
        // The ONLY values accepted are `"t"` and `"f"`, because PostgreSQL's
        // `boolout` emits exactly those two. The wider spellings node-pg's default
        // parser tolerates (`true`, `yes`, `on`, `1`, ...) are `boolin` INPUT
        // syntax and never appear on the wire, so accepting them would only widen
        // what a mistyped column can smuggle through. NULL never reaches here:
        // node-pg's row parser short-circuits a -1-length field to `null` before
        // calling any type parser.
        return parseBoolStrict;
      }
      if (oid === OID_INT8 || oid === OID_NUMERIC) {
        // Exact integer / decimal: cross as the verbatim string. NEVER Number(x)
        // (which truncates > 2^53). This wins over any global override.
        return identityString;
      }
      if (oid === OID_INT8_ARRAY || oid === OID_NAME_ARRAY || oid === OID_TEXT_ARRAY) {
        // int8[] / name[] / text[]: decode the array literal HERE. These three share
        // one `array_out` syntax, and the elements the seam wants are verbatim
        // strings, so one parser covers all three.
        //
        // It deliberately does NOT reach into `pg.types` for the array framing or the
        // element decode. Borrowing a parser from there, which is what this used to
        // do (calling `defaults.getTypeParser(OID_TEXT_ARRAY)` for `name[]` and
        // `defaults.getTypeParser(OID_INT8_ARRAY)` for `int8[]`), is a live read of
        // the same global mutable map this object exists to be immune from: it pins
        // nothing and only moves which OID has to be poisoned. `int8[]` in particular
        // must keep yielding exact strings, since `Number` truncates above 2^53.
        //
        // `name[]` is not even in node-pg's default array-parser set, so without this
        // the catalog's `array_agg(attname)` would cross as raw `{a,b}` text and fail
        // the seam's `Vec<String>` (TextArray) decode.
        return parsePgArrayLiteral;
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
  opts: PgSessionOptions = {},
): Promise<{ hostDriver: HostDriver; client: PgClient; close: () => Promise<void> }> {
  const pg = (await import("pg")).default as unknown as PgModule;

  // Host-side net-allowlist: refuse a host not in the allowlist BEFORE connect
  // (the host owns the socket now). Shared with driver-mysql2.ts so the two cannot
  // drift again — reading only the authority here let a `?host=` parameter, which
  // `pg` honours over the authority, reach an unapproved host.
  assertHostAllowed(connectionString, opts.hostAllowlist, "pg");

  const client = new pg.Client({
    connectionString,
    // Connection-scoped parsers — immune to a global setTypeParser override.
    types: connectionScopedTypes(pg) as never,
    // TLS pin (host owns the socket now). Verifies the server cert against the
    // pinned CA; distinct from any `sslmode` in the connection URL.
    ...(opts.tlsCa ? { ssl: { ca: opts.tlsCa } } : {}),
    // Per-verb bound: `pg` applies `query_timeout` to every query on the client.
    ...(opts.queryTimeoutMs !== undefined ? { query_timeout: opts.queryTimeoutMs } : {}),
  });
  await client.connect();

  // `pg` emits `error` on the Client when the backend goes away (an administrator
  // terminate, a failover, a server restart) and Node turns an unhandled `error`
  // event into an uncaught exception. Without this listener the CLI dies with a raw
  // Node stack trace the instant a connection drops, before the engine's own
  // cleanup and error reporting get to run. Nothing is swallowed: `pg` rejects the
  // in-flight query with the same error, and every query issued afterwards fails
  // with "Client has encountered a connection error and is not queryable", so the
  // engine still sees, reports and cleans up after the loss.
  client.on("error", () => {});

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
