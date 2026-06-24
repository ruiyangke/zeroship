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
  // The engine-synthesized portable split helper (§9). `delim` is a single-ASCII
  // character literal; `n` is a positive integer literal in 1..8. Builds a
  // `fnSynth(splitPart, …)` node (NEVER SQL text) and LINTS the envelope at record
  // time so the AI loop gets the structured EXPR_NOT_PORTABLE feedback EARLY (the
  // peer of the Rust validator's authoritative gate; the boundary is enforced on
  // BOTH sides). Out of envelope ⇒ throw EXPR_NOT_PORTABLE.
  splitPart: (col, delim, n) => {
    splitPartEnvelopeLint(delim, n);
    return {
      node: "fnSynth",
      fn: "splitPart",
      args: [normalizeExprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    };
  },
};

/// The `c.fn` namespace — the scalar-function helpers reached off the fluent column
/// builder (§3.1 / §9). The ONLY split surface (`c.fn.splitPart`); there is no
/// author-named `split_part`/`substr`/`instr`, and no raw escape — an exotic split
/// is simply not expressible (property A). Mirrors the `e.*` helper set; kept here
/// so the §3.1 hero `c.fn.splitPart(c("name"), " ", 1)` shape is authorable.
export const cFn = {
  splitPart: e.splitPart,
  concatWs: (delim, ...values) => e.fnSynth("concatWs", [normalizeExprArg(delim), ...values.map(normalizeExprArg)]),
  coalesce: (...args) => e.fnCall("coalesce", args.map(normalizeExprArg)),
};

/// The max literal part index `c.fn.splitPart` admits — the O(2ⁿ) inline-unroll
/// bound (§9). MUST equal the Rust `SPLIT_PART_MAX_N` (and `dml::SPLIT_PART_MAX_N`);
/// the cross-side envelope is gated on both.
const SPLIT_PART_MAX_N = 8;

/// LINT the `c.fn.splitPart` envelope (§9): the delimiter must be a single ASCII
/// character (one byte, code point < 0x80), and `n` a positive integer literal in
/// 1..=SPLIT_PART_MAX_N. Out of envelope ⇒ throw a structured EXPR_NOT_PORTABLE
/// error (the §8.8 machine-readable rejection the AI loop self-corrects on). This
/// is the JS peer of the Rust `validate::check_split_part` gate — the boundary is
/// enforced on BOTH sides so the AI loop is told at record time, not only at load.
function splitPartEnvelopeLint(delim, n) {
  const fail = (reason) => {
    const err = new Error(reason);
    err.code = "EXPR_NOT_PORTABLE";
    err.suggested_fix =
      "use a single-ASCII delimiter with 1<=n<=8, restructure to stay in-envelope " +
      "(split into <=8 parts), or mark the migration PG-only (dialect_scope=PgOnly)";
    throw err;
  };
  if (typeof delim !== "string") {
    fail(`c.fn.splitPart delimiter must be a single-ASCII string literal; got ${typeof delim}`);
  }
  // A single ASCII BYTE: exactly one UTF-16 code unit AND code point < 0x80.
  if (delim.length !== 1 || delim.charCodeAt(0) >= 0x80) {
    fail(`c.fn.splitPart delimiter must be a single ASCII character (one byte, code point < 0x80); got ${JSON.stringify(delim)}`);
  }
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`c.fn.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if (n < 1) {
    fail(`c.fn.splitPart part index n must be a positive integer; got ${n}`);
  }
  if (n > SPLIT_PART_MAX_N) {
    fail(`c.fn.splitPart part index n must be <= ${SPLIT_PART_MAX_N} (the proven inline-unroll bound); got ${n}`);
  }
}

/// Coerce a `splitPart`/`concatWs` argument: a bare string is a ColRef shorthand
/// (`c("name")` ⇒ a colRef node); an already-built `{node:…}` passes through.
function normalizeExprArg(arg) {
  if (arg && typeof arg === "object" && typeof arg.node === "string") {
    return arg;
  }
  if (typeof arg === "string") {
    return { node: "colRef", name: arg };
  }
  return { node: "literal", value: arg };
}
