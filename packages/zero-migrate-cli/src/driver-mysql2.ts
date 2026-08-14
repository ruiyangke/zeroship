// Host MySQL driver — TLS-pin + net-allowlist + timeout logic live HOST-side (the
// host owns the socket). Uses the host's `mysql2/promise`.
//
// Same `hostDriver([request, done]) => void` contract as `driver-pg.ts`. ONE pinned
// connection per session (the addon is strictly one-verb-at-a-time). Exact
// integers: mysql2 returns BIGINT as a JS string when `supportBigNumbers` +
// `bigNumberStrings` are set, so `event_seq`/`version` cross exactly.
//
// `mysql2` is an optionalDependency — imported lazily so a PG/SQLite-only
// host never needs it installed.

type Mysql2Module = typeof import("mysql2/promise");
type Mysql2Connection = import("mysql2/promise").Connection;

// The neutral cell DTOs come from the GENERATED addon `index.d.ts` (via `addon.ts`)
// — the single source of truth. No hand-copied interfaces.
import type { JsCell, JsRow, JsRequest, JsReply, JsError } from "./addon.js";
import { assertHostAllowed } from "./net-allowlist.js";

export type MysqlHostDriver = (
  args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void],
) => void;

/** TLS + allowlist options the host enforces (the host owns the socket). */
export interface MysqlSessionOptions {
  /** A CA bundle to pin (TLS). When set, `mysql2` verifies the server cert. */
  tlsCa?: string;
  /** Reject the connection if the resolved host is not in this allowlist (a bare
   *  host-side net-allowlist — the host owns the socket now). */
  hostAllowlist?: string[];
  /** Per-verb timeout in ms (the addon has its own watchdog for the shadow path;
   *  this bounds a single query). */
  queryTimeoutMs?: number;
}

/** Session semantics required before any host-side authored statement runs. */
export const MYSQL_SESSION_SQL_MODE_PIN =
  "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, 'NO_BACKSLASH_ESCAPES', 'NO_AUTO_VALUE_ON_ZERO')";

/**
 * Open a pinned host MySQL session and return the `hostDriver` callback + `close()`.
 * BIGINT crosses as a string (exact-integer domain).
 */
export async function openMysqlSession(
  url: string,
  opts: MysqlSessionOptions = {},
): Promise<{ hostDriver: MysqlHostDriver; connection: Mysql2Connection; close: () => Promise<void> }> {
  const mysql = (await import("mysql2/promise")) as unknown as Mysql2Module;

  // Host-side net-allowlist: refuse a host not in the allowlist BEFORE connect.
  // Shared with driver-pg.ts. mysql2 ignores a `?host=` parameter today, but the
  // check covers it anyway: if that ever changes, an unapproved host fails closed
  // instead of quietly becoming reachable.
  assertHostAllowed(url, opts.hostAllowlist, "mysql");

  const connection = await mysql.createConnection({
    uri: url,
    // Exact-integer domain: BIGINT / DECIMAL cross as strings.
    supportBigNumbers: true,
    bigNumberStrings: true,
    decimalNumbers: false,
    // TLS pin (host owns the socket now).
    ...(opts.tlsCa ? { ssl: { ca: opts.tlsCa } } : {}),
    multipleStatements: true, // the engine issues multi-statement DDL batches.
  });

  // The Rust backend repeats this before every author DDL/data step. Pin it at
  // connection creation as well so every host-side parameterized verb shares
  // the same literal semantics from its first request onward. The explicit-zero
  // mode is also the fail-safe for legacy identity imports: `0` must remain `0`,
  // never become an implicit AUTO_INCREMENT allocation.
  await connection.query(MYSQL_SESSION_SQL_MODE_PIN);

  const hostDriver: MysqlHostDriver = ([request, done]) => {
    runVerb(connection, request, opts.queryTimeoutMs).then(
      (reply) => done(null, reply),
      (err: unknown) => done(toJsError(err), null),
    );
  };

  return {
    hostDriver,
    connection,
    close: async () => {
      await connection.end();
    },
  };
}

async function runVerb(
  connection: Mysql2Connection,
  request: JsRequest,
  timeoutMs?: number,
): Promise<JsReply> {
  switch (request.kind) {
    case "batch": {
      await connection.query({ sql: request.sql, timeout: timeoutMs });
      return { rows: [], rowCount: undefined };
    }
    case "execute": {
      const [result] = await connection.execute(
        { sql: request.sql, timeout: timeoutMs },
        cellsToParams(request.binds),
      );
      return { rows: [], rowCount: affectedRows(result) };
    }
    case "executeTextParams": {
      // Text-format params: cross verbatim; null → SQL NULL. mysql2 binds strings.
      const values = request.textParams.map((v) => (v === null || v === undefined ? null : v));
      const [result] = await connection.execute({ sql: request.sql, timeout: timeoutMs }, values);
      return { rows: [], rowCount: affectedRows(result) };
    }
    case "query":
    case "queryOne": {
      const [rows, fields] = await connection.execute({
        sql: request.sql,
        timeout: timeoutMs,
        rowsAsArray: true,
      }, cellsToParams(request.binds));
      const columns = (fields as Array<{ name: string; columnType?: number }> | undefined)?.map((f) => f.name) ?? [];
      const types = (fields as Array<{ name: string; columnType?: number }> | undefined)?.map((f) => f.columnType ?? -1) ?? [];
      const jsRows: JsRow[] = (rows as unknown[][]).map((arr) => ({
        columns,
        cells: arr.map((v, i) => valueToCell(v, types[i])),
      }));
      return { rows: jsRows, rowCount: (rows as unknown[]).length };
    }
    default:
      throw new Error(`host mysql driver: unknown verb kind ${JSON.stringify(request.kind)}`);
  }
}

/** mysql2 result → affected rows. */
function affectedRows(result: unknown): number | undefined {
  if (result && typeof result === "object" && "affectedRows" in result) {
    const n = (result as { affectedRows: unknown }).affectedRows;
    return typeof n === "number" ? n : undefined;
  }
  return undefined;
}

/** Convert exact engine cells to mysql2 parameters. Exported for the driver
 * conformance test; it is not part of the package's public root API. */
export function cellsToParams(
  binds: JsCell[],
): Array<string | number | bigint | boolean | null> {
  return binds.map((cell) => {
    switch (cell.kind) {
      case "null":
        return null;
      case "text":
        return cell.text ?? null;
      case "int":
        // mysql2 quotes JavaScript strings. That is harmless for most numeric
        // columns but invalid in syntax-sensitive numeric positions such as
        // `LIMIT ?`. BigInt stays exact and mysql2 formats it as an unquoted
        // integer token.
        return cell.intStr !== undefined && cell.intStr !== null
          ? BigInt(cell.intStr)
          : cell.int ?? null;
      case "bool":
        return cell.bool ?? null;
      case "textArray":
        // MySQL has no array type; the engine never binds a text[] on the MySQL
        // path. Cross as a JSON string defensively.
        return JSON.stringify(cell.textArray ?? []);
      default:
        return null;
    }
  });
}

// mysql2 field columnType codes for the exact-integer domain (LONGLONG=8, NEWDECIMAL=246).
const MYSQL_LONGLONG = 8;
const MYSQL_NEWDECIMAL = 246;
const MYSQL_TINY = 1; // often BOOL

function valueToCell(value: unknown, columnType: number): JsCell {
  if (value === null || value === undefined) return { kind: "null" };
  if (columnType === MYSQL_LONGLONG || columnType === MYSQL_NEWDECIMAL) {
    // BIGINT / DECIMAL: crossed as a STRING (bigNumberStrings) → exact `intStr`.
    return { kind: "int", intStr: String(value) };
  }
  if (typeof value === "boolean") return { kind: "bool", bool: value };
  if (typeof value === "bigint") return { kind: "int", intStr: value.toString() };
  if (typeof value === "number") return { kind: "int", int: value };
  if (Array.isArray(value)) {
    return { kind: "textArray", textArray: value.map((el) => (el == null ? null : String(el))) };
  }
  if (value instanceof Date) return { kind: "text", text: value.toISOString() };
  if (typeof value === "string") return { kind: "text", text: value };
  if (Buffer.isBuffer(value)) return { kind: "text", text: value.toString("utf8") };
  return { kind: "text", text: JSON.stringify(value) };
}

function toJsError(err: unknown): JsError {
  if (err && typeof err === "object") {
    const e = err as { message?: unknown; code?: unknown; sqlState?: unknown };
    return {
      message: typeof e.message === "string" ? e.message : String(err),
      code: typeof e.sqlState === "string" ? e.sqlState : typeof e.code === "string" ? e.code : undefined,
    };
  }
  return { message: String(err) };
}
