// The minimal `@zeroship/migrate` op.* recorder DSL (design §2.1 / §2.5 / PR1
// "skeletal JS builder").
//
// This is the SKELETAL builder the PR1 anti-drift gate hinges on: it is "just
// enough to emit the golden corpus — so the byte-equality gate exists at the
// moment the IR shape is frozen" (PR1 bullet, normative §2.5). It is NOT the full
// fluent-column-accessor expression builder (§3.3.1) — that lands with the DML
// waves (PR6a). Here every op-function records the SAME dialect-neutral op object
// the Rust `Op` enum deserializes, and an Expr slot is authored as the closed-AST
// node object directly (the `e.*` helpers below), so a fixture can carry a `where`
// / `set` without the full fluent surface.
//
// CONTRACT: each named import is an op-function that PUSHES one canonical op
// object onto the module-local recording buffer (`__ops`). A migration module
// calls these inside `up()` (and optionally `down()`); the adapter
// (`op_recorder.js`) drains the buffer after invoking `up()` and emits the
// `.ir.json` envelope. The JS side does NOT compute the checksum — the Rust engine
// is the single checksum authority (§2.4 point 2 / §2.5); the JS emits ops, Rust
// folds `Checksum::of_ir`.
//
// WIRE SHAPE: the op-region fields are camelCase (`ifExists`, `cursorColumn`,
// `batchSize`, `referencesTable`), matching the frozen `op-ir.schema.json` (the
// `ir_wire_contract.rs` casing pins). An absent optional is OMITTED (never
// `field: undefined`/`null`) so the JCS image matches the Rust `skip_serializing_if`
// omitted-key image (§2.5 cross-impl determinism).

const __ops = [];

/** Drain + return the recorded op list (the adapter calls this). */
export function __drain() {
  const out = __ops.slice();
  __ops.length = 0;
  return out;
}

/** Drop keys whose value is `undefined` so an absent optional is OMITTED on the
 *  wire (never `"k":null`) — the cross-impl determinism contract (§2.5). */
function compact(obj) {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function push(op) {
  __ops.push(op);
  return op;
}

// ---------------------------------------------------------------------------
// DDL op-functions (PR1 scope).
// ---------------------------------------------------------------------------

export function createTable(name, columns, opts = {}) {
  return push(compact({
    op: "createTable",
    name,
    columns,
    constraints: opts.constraints,
    indexes: opts.indexes,
  }));
}

export function dropTable(table, opts = {}) {
  return push(compact({
    op: "dropTable",
    table,
    ifExists: opts.ifExists,
    cascade: opts.cascade,
  }));
}

export function addColumn(table, column, type, opts = {}) {
  return push(compact({
    op: "addColumn",
    table,
    column,
    type,
    nullable: opts.nullable,
    default: opts.default,
  }));
}

export function dropColumn(table, column, opts = {}) {
  return push(compact({
    op: "dropColumn",
    table,
    column,
    ifExists: opts.ifExists,
  }));
}

export function createIndex(table, columns, opts = {}) {
  return push(compact({
    op: "createIndex",
    table,
    columns,
    name: opts.name,
    unique: opts.unique,
    using: opts.using,
    where: opts.where,
    concurrently: opts.concurrently,
  }));
}

export function dropIndex(name, opts = {}) {
  return push(compact({
    op: "dropIndex",
    name,
    table: opts.table,
    unique: opts.unique,
    ifExists: opts.ifExists,
    concurrently: opts.concurrently,
  }));
}

export function alterColumnType(table, column, type, opts = {}) {
  return push(compact({
    op: "alterColumnType",
    table,
    column,
    type,
    using: opts.using,
  }));
}

export function alterColumnNullability(table, column, nullable) {
  return push({ op: "alterColumnNullability", table, column, nullable });
}

export function renameColumn(table, from, to, type) {
  return push({ op: "renameColumn", table, from, to, type });
}

export function addConstraint(table, constraint) {
  return push({ op: "addConstraint", table, constraint });
}

export function dropConstraint(table, name) {
  return push({ op: "dropConstraint", table, name });
}

// ---------------------------------------------------------------------------
// DML op-functions (the OP SHAPE is frozen in PR1 so the corpus + exhaustiveness
// gate cover every variant; the executors land in PR6a/PR6b).
// ---------------------------------------------------------------------------

export function insert(table, columns, rows, opts = {}) {
  // §PR6a — the optional `onConflict` upsert facet. PostgreSQL-ONLY: it renders
  // natively on PG and is a hard authoring error on SQLite (dialect_scope=PgOnly,
  // §9). The wire shape is `{ columns: string[], doUpdate?: { col: scalar } }`;
  // absent `doUpdate` ⇒ `DO NOTHING`. Omitted entirely ⇒ a plain (portable) insert.
  return push(compact({ op: "insert", table, columns, rows, onConflict: opts.onConflict }));
}

export function update(table, set, opts = {}) {
  return push(compact({
    op: "update",
    table,
    set,
    where: opts.where,
    batch: opts.batch,
  }));
}

// `del` (not `delete` — the JS reserved word); records the `"delete"` discriminant
// per the ADR (`docs/decisions/2026-06-23-op-ir-serde-repr.md`).
export function del(table, where, opts = {}) {
  return push(compact({ op: "delete", table, where, limit: opts.limit }));
}

export function backfill(table, cursorColumn, batchSize, set, name, opts = {}) {
  return push(compact({
    op: "backfill",
    table,
    cursorColumn,
    batchSize,
    set,
    filter: opts.filter,
    name,
  }));
}

// ---------------------------------------------------------------------------
// Closed expression-AST node helpers (`e.*`) — the SKELETAL stand-in for the
// fluent `(c) => Expr` builder (§3.3.1), enough for a fixture to carry a closed
// `where`/`set`/partial-index predicate. Each returns the SAME `{"node":…}` object
// the Rust `Expr` enum deserializes (camelCase, internally tagged on `"node"`).
// ---------------------------------------------------------------------------

export const e = {
  col: (name) => ({ node: "colRef", name }),
  lit: (value) => ({ node: "literal", value }),
  binOp: (op, lhs, rhs) => ({ node: "binOp", op, lhs, rhs }),
  unaryOp: (op, operand) => ({ node: "unaryOp", op, operand }),
  fnCall: (fn, args) => ({ node: "fnCall", fn, args }),
  fnSynth: (fn, args) => ({ node: "fnSynth", fn, args }),
  cast: (operand, target) => ({ node: "cast", operand, target }),
};
