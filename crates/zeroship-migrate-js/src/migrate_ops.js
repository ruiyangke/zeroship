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
  // The engine-synthesized portable split helper (§9). `delim` is a string literal;
  // `n` is a positive integer literal. Builds a `fnSynth(splitPart, …)` node (NEVER
  // SQL text) and LINTS the GRAMMAR at record time so the AI loop gets structured
  // feedback EARLY for a genuinely-malformed node (a non-string/empty delim, a
  // non-positive-int n) — broken on BOTH dialects. The portability ENVELOPE
  // (single-ASCII delim, 1<=n<=8) is NOT enforced here: it is DIALECT-GATED and the
  // record-time JS recorder is dialect-neutral, so an out-of-envelope splitPart is
  // a VALID node on a Postgres target (`dialect_scope=PgOnly`, §2.4.1/§9 — PG's
  // native split_part is multi-char/any-n capable). The authoritative dialect-aware
  // verdict belongs to the Rust `validate::check_split_part` (which admits it on PG,
  // rejects it on SQLite); forcing an unconditional throw here would make the
  // documented PgOnly escape non-constructible. So the builder defers the envelope.
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
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

/// LINT the `c.fn.splitPart` GRAMMAR (§9) — the dialect-NEUTRAL subset that is
/// broken on BOTH backends, so it is safe (and correct) to reject at record time:
/// the delimiter must be a NON-EMPTY string literal, and `n` a POSITIVE integer
/// literal. A violation throws a structured EXPR_NOT_PORTABLE error (the §8.8
/// machine-readable rejection the AI loop self-corrects on).
///
/// The portability ENVELOPE (single-ASCII delimiter, `1 <= n <= 8`) is NOT checked
/// here — it is dialect-gated, and the record-time recorder is dialect-neutral. An
/// out-of-envelope splitPart is a valid node on a Postgres target (the documented
/// `dialect_scope=PgOnly` escape, §2.4.1/§9); the authoritative dialect-aware
/// verdict is the Rust `validate::check_split_part` (admit on PG, reject on SQLite).
/// Enforcing the envelope here would make the PgOnly escape non-constructible.
function splitPartGrammarLint(delim, n) {
  const fail = (reason) => {
    const err = new Error(reason);
    err.code = "EXPR_NOT_PORTABLE";
    err.suggested_fix =
      "pass a non-empty string-literal delimiter and a positive-integer n; to target " +
      "SQLite too, stay in-envelope (single-ASCII delimiter, 1<=n<=8) — a multi-char/" +
      "non-ASCII delimiter or n>8 renders only on Postgres (dialect_scope=PgOnly)";
    throw err;
  };
  if (typeof delim !== "string") {
    fail(`c.fn.splitPart delimiter must be a string literal; got ${typeof delim}`);
  }
  if (delim.length === 0) {
    fail("c.fn.splitPart delimiter must be a non-empty string literal");
  }
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`c.fn.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if (n < 1) {
    fail(`c.fn.splitPart part index n must be a positive integer; got ${n}`);
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
