// Host MySQL driver (design §D.2) — the existing `JsDriverBackend` protocol minus
// the in-Rust V8: TLS-pin + net-allowlist + timeout logic move HOST-side (the host
// now owns the socket). Uses the host's `mysql2/promise`.
//
// Same `hostDriver([request, done]) => void` contract as `driver-pg.ts`. ONE pinned
// connection per session (the addon is strictly one-verb-at-a-time, §B.6). Exact
// integers: mysql2 returns BIGINT as a JS string when `supportBigNumbers` +
// `bigNumberStrings` are set, so `event_seq`/`version` cross exactly (§D.2).
//
// `mysql2` is an optionalDependency (§D.3/§E) — imported lazily so a PG/SQLite-only
// host never needs it installed.

type Mysql2Module = typeof import("mysql2/promise");
type Mysql2Connection = import("mysql2/promise").Connection;

interface JsCell {
  kind: "null" | "text" | "int" | "bool" | "textArray";
  text?: string;
  int?: number;
  intStr?: string;
  bool?: boolean;
  textArray?: Array<string | null | undefined>;
}

interface JsRow {
  columns: string[];
  cells: JsCell[];
}

interface JsRequest {
  kind: "batch" | "execute" | "executeTextParams" | "query" | "queryOne";
  sql: string;
  binds: JsCell[];
  textParams: Array<string | null | undefined>;
}

interface JsReply {
  rows: JsRow[];
  rowCount?: number;
}

interface JsError {
  message: string;
  code?: string;
}

export type MysqlHostDriver = (
  args: [request: JsRequest, done: (err: JsError | null, reply: JsReply | null) => void],
) => void;

/** TLS + allowlist options the host enforces (moved out of the in-Rust V8, §D.2). */
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

/**
 * Open a pinned host MySQL session and return the `hostDriver` callback + `close()`.
 * BIGINT crosses as a string (exact-integer domain, §D.2).
 */
export async function openMysqlSession(
  url: string,
  opts: MysqlSessionOptions = {},
): Promise<{ hostDriver: MysqlHostDriver; connection: Mysql2Connection; close: () => Promise<void> }> {
  const mysql = (await import("mysql2/promise")) as unknown as Mysql2Module;

  // Host-side net-allowlist (§D.2): refuse a host not in the allowlist BEFORE connect.
  if (opts.hostAllowlist && opts.hostAllowlist.length > 0) {
    const parsed = new URL(url);
    if (!opts.hostAllowlist.includes(parsed.hostname)) {
      throw new Error(
        `host mysql driver: ${parsed.hostname} is not in the host allowlist ${JSON.stringify(opts.hostAllowlist)}`,
      );
    }
  }

  const connection = await mysql.createConnection({
    uri: url,
    // Exact-integer domain: BIGINT / DECIMAL cross as strings (§D.2).
    supportBigNumbers: true,
    bigNumberStrings: true,
    decimalNumbers: false,
    // TLS pin (host owns the socket now).
    ...(opts.tlsCa ? { ssl: { ca: opts.tlsCa } } : {}),
    multipleStatements: true, // the engine issues multi-statement DDL batches.
  });

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
      const [result] = await connection.query(
        { sql: request.sql, timeout: timeoutMs },
        cellsToParams(request.binds),
      );
      return { rows: [], rowCount: affectedRows(result) };
    }
    case "executeTextParams": {
      // Text-format params: cross verbatim; null → SQL NULL. mysql2 binds strings.
      const values = request.textParams.map((v) => (v === null || v === undefined ? null : v));
      const [result] = await connection.query({ sql: request.sql, timeout: timeoutMs }, values);
      return { rows: [], rowCount: affectedRows(result) };
    }
    case "query":
    case "queryOne": {
      const [rows, fields] = await connection.query({
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

function cellsToParams(binds: JsCell[]): unknown[] {
  return binds.map((cell) => {
    switch (cell.kind) {
      case "null":
        return null;
      case "text":
        return cell.text ?? null;
      case "int":
        return cell.intStr ?? cell.int ?? null;
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
