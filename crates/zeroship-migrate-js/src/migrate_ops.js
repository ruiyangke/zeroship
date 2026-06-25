// The `@zeroship/migrate` op-builder DSL — the FULL §3.2/§3.3.1 surface
// (PR3). This is the recorder the runtime evaluates in V8 to turn a creator's
// `import { createTable, addColumn, … } from "@zeroship/migrate"` migration into
// the canonical `.ir.json` the lean engine deserializes.
//
// HISTORY: PR1 shipped a SKELETAL subset of this builder (just enough to emit the
// golden corpus at IR-freeze time) with POSITIONAL op signatures and an `e.*`
// closed-AST node helper. PR3 fleshes it to the COMPLETE locked surface
// (normative §3.2/§3.3.1):
//
//   - named ESM imports, no `op` object/prefix; `export default { name?, up, down? }`
//     (the recorder adapter `op_recorder.js` resolves either shape);
//   - the chainable `t.*` column-type lexicon (`t.text().notNull().default(…)`,
//     nullable-by-default, plus an options-bag overload), one `ColumnDef`
//     representation everywhere;
//   - `createTable(name, { col: t.text() })` (object literal — no thunk) plus the
//     `(b) => { … }` scoped-builder overload for table-scoped constraints/indexes;
//   - `(table, spec)` constraint/index adders with `name` always in the spec and
//     named `references.{table,columns}` (no transposable positionals);
//   - `verb(table, { … })` DML with one `where` keyword across `update`/`del`/
//     `backfill`, and `insert(table, { rows, onConflict? })`;
//   - the single-handle fluent `(c) => Expr` builder: `c("name")` → `ColRef`, the
//     chainable operator methods, and the `c.fn.*` scalar-function namespace (no
//     importable `fn`, no second callback arg);
//   - `batchAlterTable(table, build)` (SQLite-safe-rebuild scoped builder).
//
// BACK-COMPAT NOTE: PR3 ships pre-launch (no published users — AGENTS.md). The
// PR1 POSITIONAL/`e.*` authoring style is NOT a public contract, but the existing
// golden corpus + `split_part_lint.rs` author in it, so the op-functions accept
// BOTH the new fluent forms and the legacy forms. Both authoring styles emit the
// IDENTICAL wire op object — the `.ir.json` shape is frozen and dialect-neutral.
// The two styles are disambiguated structurally (an array `columns` arg is the
// legacy `createTable`; an object is the fluent map; a `ColumnDef` vs a bare
// ColType string for `addColumn`'s type; etc.).
//
// CONTRACT: each named import RECORDS one canonical op object onto the
// module-local recording buffer (`__ops`), synchronously, returning void. A
// migration calls these inside `up()`/`down()`; the adapter drains the buffer per
// phase and emits the `.ir.json` envelope. Calling an op-function OUTSIDE an
// active recorder (module top level, or after `up()` returns) throws a structured
// `OP_OUTSIDE_RECORDER` (§3.1) — the op cannot be silently lost.
//
// CHECKSUM: the JS side NEVER computes the checksum (§2.4 point 2 / §2.5); the
// Rust engine is the single `Checksum::of_ir` authority. JS emits ops; Rust folds.
//
// WIRE SHAPE: the op-region fields are camelCase (`ifExists`, `cursorColumn`,
// `batchSize`, `referencesTable`), matching the frozen `op-ir.schema.json`. An
// absent optional is OMITTED (never `field: undefined`/`null`) so the JCS image
// matches the Rust `skip_serializing_if` omitted-key image (§2.5).

// ---------------------------------------------------------------------------
// The ambient per-migration recorder (§3.1). A migration's `up()`/`down()` is
// parameterless; the op-functions append onto the ACTIVE recorder. The adapter
// installs a fresh recorder before each phase. Recording OUTSIDE an active
// recorder (top level / after the phase returns) is a structured error.
// ---------------------------------------------------------------------------

let __active = null;

/** Structured error helper — mirrors the §8.8 machine-readable envelope. */
function structuredError(code, message, extra) {
  const err = new Error(message);
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

/** Begin a fresh recording buffer (called by the adapter before a phase). */
export function __begin() {
  __active = [];
}

/** Drain + return the recorded op list, clearing the active recorder (the
 *  adapter calls this after a phase). Returns `[]` if no recorder is active. */
export function __drain() {
  if (__active === null) return [];
  const out = __active;
  __active = null;
  return out;
}

function push(op) {
  if (__active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      `op-function "${op.op}" called outside an active migration recorder; ` +
        "op-functions may only be called synchronously inside up()/down() " +
        "(not at module top level or after the phase returns)",
      {
        suggested_fix:
          "move the op-function call inside the migration's up()/down() body",
      },
    );
  }
  __active.push(op);
  return op;
}

/** Drop keys whose value is `undefined` so an absent optional is OMITTED on the
 *  wire (never `"k":null`) — the cross-impl determinism contract (§2.5). */
function compact(obj) {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

// ===========================================================================
// (B) The chainable `t.*` column-type lexicon (§3.2). Every factory returns a
// CHAINABLE ColumnDef mirroring the expression chain — NULLABLE BY DEFAULT;
// `.notNull()` / `.default(x)` / `.ref(target)` / `.primaryKey()` / `.unique()`
// opt in. An options-bag overload is also accepted (`t.text({ notNull: true })`).
// There is ONE column-type representation (`ColumnDef`) everywhere — `addColumn`
// /`renameColumn`/`createTable`/`alterColumn` all consume it.
// ===========================================================================

class ColumnDef {
  /** @param {object} colType the dialect-neutral ColType wire value (§3.2). */
  constructor(colType) {
    this._type = colType;
    this._nullable = true; // nullable by default (§3.2)
    this._default = undefined; // an IrDefault wire value, or undefined
    this._primaryKey = false;
    this._unique = false;
  }

  notNull() {
    this._nullable = false;
    return this;
  }

  /** `.default(value | { fn: "now" | "genRandomUuid" })` → a structured IrDefault
   *  (typed literal OR nullary synth scalar) — NEVER raw SQL (property A). */
  default(value) {
    this._default = toIrDefault(value);
    return this;
  }

  /** Re-target a column as a foreign-key reference (`t.text().ref("users")` or
   *  `t.uuid().ref("users")`): rewrites the ColType to the `{ref:{references}}`
   *  wire form. */
  ref(targetTable) {
    requireString(targetTable, "t.*.ref(target)");
    this._type = { ref: { references: targetTable } };
    return this;
  }

  primaryKey() {
    this._primaryKey = true;
    this._nullable = false; // a PK column is implicitly NOT NULL
    return this;
  }

  unique() {
    this._unique = true;
    return this;
  }

  /** Reduce to an `IrColumn` (the `createTable` columns[] / shape). `name` is the
   *  map key. `nullable`/`default`/`unique` omitted when at their defaults so the
   *  wire image matches the Rust `skip_serializing_if`. */
  __toIrColumn(name) {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      unique: this._unique ? true : undefined,
    });
  }

  /** Reduce to the `addColumn` op tail (`{ type, nullable?, default? }`). */
  __toAddColumnTail() {
    return compact({
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
    });
  }
}

/** Marker the op-functions use to tell a fluent `ColumnDef` from a legacy bare
 *  ColType string / wire object. */
function isColumnDef(x) {
  return x instanceof ColumnDef;
}

/** Apply the options-bag overload (`t.text({ notNull, default, primaryKey,
 *  unique, ref })`) onto a fresh ColumnDef. */
function applyOpts(def, opts) {
  if (opts === undefined || opts === null) return def;
  if (typeof opts !== "object") {
    throw structuredError("OP_INVALID", "t.* options bag must be an object");
  }
  if (opts.notNull) def.notNull();
  if (opts.primaryKey) def.primaryKey();
  if (opts.unique) def.unique();
  if (opts.ref !== undefined) def.ref(opts.ref);
  if (opts.default !== undefined) def.default(opts.default);
  return def;
}

/** Base64-encode raw bytes (the `IrScalar::Bytes` wire carrier) without a Node
 *  `Buffer` — `btoa` is a WHATWG global present in the V8 record host + Node. */
function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/** Normalize a JS scalar into the closed `IrScalar` WIRE carrier so the recorded
 *  shape is exactly what Rust's `IrScalar` deserializer accepts (§2.5/§3.2):
 *   - a JS `bigint` → `{ decimal: "<v>" }` (a bare bigint THROWS at JSON.stringify;
 *     `{decimal}` is the integers-beyond-2^53 carrier);
 *   - a `Uint8Array` → `{ bytes: "<base64>" }` (the raw-bytes carrier; the default
 *     JSON spelling `{"0":…}` is HARD-REJECTED by the Rust deserializer);
 *   - everything else (string / safe number / boolean / null / the explicit
 *     `{decimal}` / `{bytes}` carriers) passes through verbatim. */
function toIrScalar(value) {
  if (typeof value === "bigint") return { decimal: value.toString() };
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  return value;
}

/** Coerce a `.default(value)` arg into the closed `IrDefault` carrier:
 *   - `{ fn: "now" | "genRandomUuid" }` → a nullary synth default;
 *   - any other typed scalar → a `{ literal: { value } }` literal default (the
 *     value carried through the `IrScalar` wire normalizer). */
function toIrDefault(value) {
  if (value && typeof value === "object" && typeof value.fn === "string") {
    return { fn: { fn: value.fn } };
  }
  return { literal: { value: toIrScalar(value) } };
}

/** The fluent column-type lexicon (§3.2). Shared in shape with `@zeroship/db`'s
 *  `t` (PR5 wires the actual shared lexicon); here it is migrate's own, emitting
 *  the dialect-neutral ColType wire forms `op-ir.schema.json` enumerates. */
export const t = {
  // The headline §3.2 set.
  /** A conventional primary-key id: a non-null UUID PK defaulting to a DB-evaluated
   *  `gen_random_uuid()` (the structured FnSynth default, never a frozen literal). */
  id: () => {
    const d = new ColumnDef("uuid");
    d.primaryKey();
    d.default({ fn: "genRandomUuid" });
    return d;
  },
  text: (opts) => applyOpts(new ColumnDef("text"), opts),
  /** Fixed-precision decimal (§3.2 `numeric`). Defaults to (38, 9); pass
   *  `t.numeric(precision, scale)` to size it. */
  numeric: (precision = 38, scale = 9, opts) =>
    applyOpts(new ColumnDef({ decimal: { precision, scale } }), opts),
  timestamp: (opts) => applyOpts(new ColumnDef("timestamp"), opts),
  uuid: (opts) => applyOpts(new ColumnDef("uuid"), opts),
  bytes: (opts) => applyOpts(new ColumnDef("bytea"), opts),
  boolean: (opts) => applyOpts(new ColumnDef("bool"), opts),
  json: (opts) => applyOpts(new ColumnDef("json"), opts),
  /** A foreign-key reference column carrying a plain-string target table name
   *  (NOT live-schema-bound — §3.3). */
  ref: (targetTable, opts) => {
    requireString(targetTable, "t.ref(target)");
    return applyOpts(new ColumnDef({ ref: { references: targetTable } }), opts);
  },
  vector: (n, opts) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    return applyOpts(new ColumnDef({ vector: { vector: n } }), opts);
  },
  geoPoint: (opts) => applyOpts(new ColumnDef("geoPoint"), opts),

  // The remaining closed ColType set so the `t.*` lexicon can express ANY column
  // type the IR supports (the task's "author any op the Rust IR supports").
  string: (opts) => applyOpts(new ColumnDef("string"), opts),
  int: (opts) => applyOpts(new ColumnDef("int"), opts),
  integer: (opts) => applyOpts(new ColumnDef("int"), opts),
  bigInt: (opts) => applyOpts(new ColumnDef("bigInt"), opts),
  float: (opts) => applyOpts(new ColumnDef("float"), opts),
  /** An application-level encrypted column wrapping an inner `t.*` type
   *  (`t.encrypted({ of: t.text() })`). */
  encrypted: (arg, opts) => {
    const inner = arg && arg.of !== undefined ? arg.of : arg;
    const innerType = isColumnDef(inner) ? inner._type : inner;
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return applyOpts(new ColumnDef({ encrypted: { of: innerType } }), opts);
  },
};

/** Resolve a column-type argument to its ColType wire value. Accepts a fluent
 *  `ColumnDef` (the §3.2 form) OR a bare ColType string/object (legacy/wire). */
function colTypeOf(typeArg) {
  if (isColumnDef(typeArg)) return typeArg._type;
  return typeArg;
}

function requireString(v, what) {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

// ===========================================================================
// (B continued) The single-handle fluent `(c) => Expr` builder (§3.3.1). `c` is
// BOTH a column-accessor function (`c("name")` → a chainable ColRef) and the
// `c.fn.*` scalar-function namespace. The chain auto-wraps bare JS values to
// `Literal`. Every method builds exactly one closed-AST node; the recorder
// captures it as data — the engine owns all per-dialect rendering.
// ===========================================================================

/** Wrap a closed-AST node object in a chainable `ExprChain` so operator methods
 *  hang off any sub-expression (a ColRef, a literal, a fn result, …). */
function chain(node) {
  return new ExprChain(node);
}

/** Auto-wrap a bare JS value to a `Literal` node; pass a chain/node through. */
function exprArg(x) {
  if (x instanceof ExprChain) return x.__node;
  if (x && typeof x === "object" && typeof x.node === "string") return x; // a raw AST node
  return { node: "literal", value: x };
}

class ExprChain {
  constructor(node) {
    this.__node = node;
  }

  // ── comparison ──
  eq(x) { return chain({ node: "binOp", op: "eq", lhs: this.__node, rhs: exprArg(x) }); }
  ne(x) { return chain({ node: "binOp", op: "ne", lhs: this.__node, rhs: exprArg(x) }); }
  lt(x) { return chain({ node: "binOp", op: "lt", lhs: this.__node, rhs: exprArg(x) }); }
  le(x) { return chain({ node: "binOp", op: "le", lhs: this.__node, rhs: exprArg(x) }); }
  gt(x) { return chain({ node: "binOp", op: "gt", lhs: this.__node, rhs: exprArg(x) }); }
  ge(x) { return chain({ node: "binOp", op: "ge", lhs: this.__node, rhs: exprArg(x) }); }

  // ── boolean ──
  and(e) { return chain({ node: "binOp", op: "and", lhs: this.__node, rhs: exprArg(e) }); }
  or(e) { return chain({ node: "binOp", op: "or", lhs: this.__node, rhs: exprArg(e) }); }
  not() { return chain({ node: "unaryOp", op: "not", operand: this.__node }); }

  // ── arithmetic ──
  add(x) { return chain({ node: "binOp", op: "add", lhs: this.__node, rhs: exprArg(x) }); }
  sub(x) { return chain({ node: "binOp", op: "sub", lhs: this.__node, rhs: exprArg(x) }); }
  mul(x) { return chain({ node: "binOp", op: "mul", lhs: this.__node, rhs: exprArg(x) }); }
  div(x) { return chain({ node: "binOp", op: "div", lhs: this.__node, rhs: exprArg(x) }); }

  // ── string/value ──
  /** Raw `||` concatenation (NULL-propagating on BOTH backends), folded over the
   *  receiver + every part. For NULL-skipping joins use `c.fn.concatWs` (§3.3.1). */
  concat(...parts) {
    let acc = this.__node;
    for (const p of parts) {
      acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    }
    return chain(acc);
  }
  concatWs(sep, ...parts) {
    return chain({
      node: "fnSynth",
      fn: "concatWs",
      args: [exprArg(sep), this.__node, ...parts.map(exprArg)],
    });
  }
  coalesce(...args) {
    return chain({ node: "fnCall", fn: "coalesce", args: [this.__node, ...args.map(exprArg)] });
  }

  // ── null/bool tests ──
  isNull() { return chain({ node: "unaryOp", op: "isNull", operand: this.__node }); }
  isNotNull() { return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node }); }
  isTrue() { return chain({ node: "unaryOp", op: "isTrue", operand: this.__node }); }
  isFalse() { return chain({ node: "unaryOp", op: "isFalse", operand: this.__node }); }

  // ── cast ──
  /** `.cast("integer" | "text" | "real" | "boolean" | "blob")` — the closed
   *  portable target set (§3.3.1); a non-portable target is rejected by the Rust
   *  validator (`UNSUPPORTED { kind:"expr" }`). */
  cast(target) {
    return chain({ node: "cast", operand: this.__node, target });
  }
}

/** Build the single fluent handle `c`: a column-accessor function carrying the
 *  `c.fn.*` namespace. `c("name")` → a chainable ColRef (a plain-string name —
 *  NOT live-schema-bound, §3.3). */
function makeBuilder() {
  const c = (name) => {
    requireString(name, 'c("name")');
    return chain({ node: "colRef", name });
  };
  c.fn = cFn; // the scalar-function namespace (§3.3.1)
  return c;
}

/** Resolve an expression slot: an `ExprFn` callback `(c) => Expr` (the §3.3.1
 *  fluent form), a chainable `ExprChain`, or a raw closed-AST node object
 *  (legacy/`e.*`). Returns the closed-AST node the wire op carries. */
function resolveExpr(slot) {
  if (slot === undefined || slot === null) return undefined;
  if (typeof slot === "function") {
    const built = slot(makeBuilder());
    return exprArg(built);
  }
  if (slot instanceof ExprChain) return slot.__node;
  if (slot && typeof slot === "object" && typeof slot.node === "string") return slot;
  throw structuredError(
    "OP_INVALID",
    "expression slot must be a (c) => Expr callback, a built expression, or a closed-AST node",
  );
}

/** Resolve a `set: { col: ExprFn }` map into a `{ col: node }` wire map. */
function resolveSet(set) {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column → expression");
  }
  const out = {};
  for (const col of Object.keys(set)) {
    out[col] = resolveExpr(set[col]);
  }
  return out;
}

// ===========================================================================
// (B continued) `c.fn.*` — the scalar-function namespace, reached off the single
// builder handle (no importable `fn`, no second callback arg — §3.3.1). Each
// member builds exactly one closed-AST node and returns a chain so the result is
// further composable. Args auto-wrap bare values to `Literal`.
// ===========================================================================

export const cFn = {
  lower: (e) => chain({ node: "fnCall", fn: "lower", args: [exprArg(e)] }),
  upper: (e) => chain({ node: "fnCall", fn: "upper", args: [exprArg(e)] }),
  trim: (e) => chain({ node: "fnCall", fn: "trim", args: [exprArg(e)] }),
  length: (e) => chain({ node: "fnCall", fn: "length", args: [exprArg(e)] }),
  abs: (e) => chain({ node: "fnCall", fn: "abs", args: [exprArg(e)] }),
  coalesce: (...args) => chain({ node: "fnCall", fn: "coalesce", args: args.map(exprArg) }),
  nullif: (a, b) => chain({ node: "fnCall", fn: "nullif", args: [exprArg(a), exprArg(b)] }),

  /** NULL-skipping `concat_ws` (PG) / `coalesce`-folded `||` (SQLite) — the safe
   *  join helper (§3.3.1). `sep` is a literal. */
  concatWs: (sep, ...parts) =>
    chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),

  /** The searched `CASE` form (`c.fn.case([[cond, val], …], elseVal)`). Each
   *  branch half + the else are themselves closed-AST nodes. */
  case: (branches, elseVal) => {
    if (!Array.isArray(branches)) {
      throw structuredError("OP_INVALID", "c.fn.case(branches, else?): branches must be an array of [cond, result]");
    }
    const node = {
      node: "case",
      branches: branches.map((b) => {
        if (!Array.isArray(b) || b.length !== 2) {
          throw structuredError("OP_INVALID", "c.fn.case branch must be a [condition, result] pair");
        }
        return { condition: exprArg(b[0]), result: exprArg(b[1]) };
      }),
    };
    if (elseVal !== undefined) node.else = exprArg(elseVal);
    return chain(node);
  },

  /** The engine-synthesized portable split helper (§9). `delim` is a string
   *  literal; `n` a positive integer literal. LINTS the dialect-NEUTRAL grammar
   *  at record time (a non-string/empty delim, a non-positive-int n — broken on
   *  BOTH backends). The portability ENVELOPE (single-ASCII delim, 1<=n<=8) is
   *  DIALECT-gated and is deferred to the Rust `validate::check_split_part`
   *  (admit on PG via dialect_scope=PgOnly, reject on SQLite); enforcing it here
   *  would make the documented PgOnly escape non-constructible. */
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },

  /** DB-evaluated apply-time scalars (the structured replacement for a frozen
   *  `Date.now()` / UUID literal, §4.3). Render to `now()` / `gen_random_uuid()`
   *  per dialect. */
  now: () => chain({ node: "fnSynth", fn: "now", args: [] }),
  genRandomUuid: () => chain({ node: "fnSynth", fn: "genRandomUuid", args: [] }),
};

// ===========================================================================
// (A) The full named-export op set (§3.2). Each records the EXACT op JSON the
// Rust closed `Op` enum / `op-ir.schema.json` deserializes (the `del` →
// `op:"delete"` wire-tag mapping, the internally-tagged `op` field, the nested
// IrDefault/IrConstraint shapes). Each accepts the fluent §3.2 form AND the
// legacy positional form, emitting the identical wire op.
// ===========================================================================

// ── DDL: tables ──

/**
 * `createTable(name, columns, opts?)` — `columns` is an object literal map
 * `{ colName: t.* }` (the fluent §3.2 form), OR the legacy `IrColumn[]` array.
 * The optional third arg is either:
 *   - a `(b) => { … }` scoped-builder callback for table-scoped constraints/
 *     indexes (the §3.2 second overload, parallel to `batchAlterTable`); OR
 *   - the legacy `{ constraints, indexes }` options bag.
 */
export function createTable(name, columns, opts, tableOptsArg) {
  requireString(name, "createTable(name, …)");

  let cols;
  const constraints = [];
  const indexes = [];

  if (Array.isArray(columns)) {
    // Legacy: a pre-built IrColumn[] array (carried verbatim).
    cols = columns;
  } else if (columns && typeof columns === "object") {
    // Fluent: a { colName: t.* } map. Reduce each ColumnDef to an IrColumn, and
    // hoist a `.primaryKey()` column into a table-level `pk` constraint.
    cols = [];
    const pkCols = [];
    for (const colName of Object.keys(columns)) {
      const def = columns[colName];
      if (!isColumnDef(def)) {
        throw structuredError(
          "OP_INVALID",
          `createTable column "${colName}" must be a t.* ColumnDef (got ${typeof def})`,
        );
      }
      cols.push(def.__toIrColumn(colName));
      if (def._primaryKey) pkCols.push(colName);
    }
    if (pkCols.length > 0) {
      constraints.push({ kind: { kind: "pk", columns: pkCols } });
    }
  } else {
    throw structuredError("OP_INVALID", "createTable columns must be a { col: t.* } map or an IrColumn[]");
  }

  if (typeof opts === "function") {
    // The scoped-builder overload: collect b.index(...) / b.unique(...) / etc.
    const b = makeTableBuilder(constraints, indexes);
    opts(b);
  } else if (opts && typeof opts === "object") {
    // Legacy options bag.
    if (Array.isArray(opts.constraints)) constraints.push(...opts.constraints);
    if (Array.isArray(opts.indexes)) indexes.push(...opts.indexes);
  }

  // **PR10** — the schema qualifier + the create-family existence guard. The
  // opts bag is the THIRD arg when it is NOT the scoped-builder callback (and
  // not the legacy {constraints,indexes} bag); read schema/ifNotExists off it.
  // **PR11** — when the THIRD arg is the `(b) => …` scoped builder OR is absent
  // (the facade passes `build = undefined`), the schema/guard bag rides on the
  // FOURTH arg (`tableOptsArg`), mirroring the TS `createTable(name, columns,
  // build?, opts)` 4-arg signature so the `table()` facade's `.create(columns,
  // build?, opts)` lowers to the IDENTICAL op. When the third arg is itself the
  // bag (the flat 3-arg call), read it there.
  const tableOpts =
    opts && typeof opts === "object" && !Array.isArray(opts)
      ? opts
      : tableOptsArg && typeof tableOptsArg === "object"
        ? tableOptsArg
        : {};
  return push(
    compact({
      op: "createTable",
      name,
      columns: cols,
      constraints: constraints.length ? constraints : undefined,
      indexes: indexes.length ? indexes : undefined,
      schema: tableOpts.schema,
      existenceGuard: ifNotExistsGuard(tableOpts.ifNotExists),
    }),
  );
}

/** The `(b) => …` scoped builder createTable's second overload passes — collects
 *  table-scoped constraints + indexes (the rare case). */
function makeTableBuilder(constraints, indexes) {
  return {
    index(columns, opts = {}) {
      indexes.push(
        compact({
          name: opts.name,
          columns,
          unique: opts.unique,
          using: opts.using,
          where: resolveExpr(opts.where),
        }),
      );
    },
    unique(columns, opts = {}) {
      constraints.push(compact({ name: opts.name, kind: { kind: "unique", columns } }));
    },
    primaryKey(columns) {
      constraints.push({ kind: { kind: "pk", columns } });
    },
    check(expr, opts = {}) {
      constraints.push(compact({ name: opts.name, kind: { kind: "check", expr: resolveExpr(expr) } }));
    },
    foreignKey(spec) {
      constraints.push(fkConstraintFromSpec(spec));
    },
  };
}

// **PR10** — map the existence-guard booleans to the wire `existenceGuard` token
// (omitted when falsy). The create/add family takes `ifNotExists`; the
// drop/rename/alter family takes `ifExists`. Engine-synthesized via a catalog
// probe — NOT a native `IF [NOT] EXISTS` clause. Replaces the old native
// `ifExists` boolean field (the intentional wire break).
function ifNotExistsGuard(v) {
  return v ? "ifNotExists" : undefined;
}
function ifExistsGuard(v) {
  return v ? "ifExists" : undefined;
}

export function dropTable(table, opts = {}) {
  requireString(table, "dropTable(table)");
  return push(
    compact({
      op: "dropTable",
      table,
      cascade: opts.cascade,
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

// ── DDL: columns ──

/**
 * `addColumn(table, name, type, opts?)`. `type` is a `t.*` ColumnDef (the §3.2
 * fluent form — its `.notNull()`/`.default()` ride along), OR a legacy bare
 * ColType string/object with the nullability/default in `opts`.
 */
export function addColumn(table, name, type, opts = {}) {
  requireString(table, "addColumn(table, …)");
  requireString(name, "addColumn(table, name, …)");
  if (isColumnDef(type)) {
    return push(
      compact({
        op: "addColumn",
        table,
        column: name,
        ...type.__toAddColumnTail(),
        schema: opts.schema,
        existenceGuard: ifNotExistsGuard(opts.ifNotExists),
      }),
    );
  }
  return push(
    compact({
      op: "addColumn",
      table,
      column: name,
      type: colTypeOf(type),
      nullable: opts.nullable,
      default: opts.default,
      schema: opts.schema,
      existenceGuard: ifNotExistsGuard(opts.ifNotExists),
    }),
  );
}

export function dropColumn(table, column, opts = {}) {
  requireString(table, "dropColumn(table, …)");
  requireString(column, "dropColumn(table, column)");
  return push(
    compact({
      op: "dropColumn",
      table,
      column,
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

/**
 * `renameColumn(table, from, to, type)` — `type` is a `t.*` ColumnDef (the
 * column type after rename, carried for re-derivation) or a legacy bare ColType.
 */
export function renameColumn(table, from, to, type, opts = {}) {
  requireString(table, "renameColumn(table, …)");
  requireString(from, "renameColumn(table, from, …)");
  requireString(to, "renameColumn(table, from, to, …)");
  return push(
    compact({
      op: "renameColumn",
      table,
      from,
      to,
      type: colTypeOf(type),
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

/**
 * `alterColumn(table, name, change)` — the §3.2 single change descriptor:
 *   - `{ type: t.* | ColType, using?: ExprFn }` → `alterColumnType`;
 *   - `{ nullable: bool }` → `alterColumnNullability`.
 * (The legacy `alterColumnType` / `alterColumnNullability` named exports remain
 * for the existing corpus.)
 */
export function alterColumn(table, name, change, opts = {}) {
  requireString(table, "alterColumn(table, …)");
  requireString(name, "alterColumn(table, name, …)");
  if (!change || typeof change !== "object") {
    throw structuredError("OP_INVALID", "alterColumn(table, name, change): change must be an object");
  }
  if (change.type !== undefined) {
    // **PR10** — carry the schema qualifier + ifExists guard through to the
    // emitted `alterColumnType` op (matching ops.ts's single emit).
    return alterColumnType(table, name, change.type, {
      using: change.using,
      schema: opts.schema,
      ifExists: opts.ifExists,
    });
  }
  if (change.nullable !== undefined) {
    return alterColumnNullability(table, name, change.nullable, {
      schema: opts.schema,
      ifExists: opts.ifExists,
    });
  }
  throw structuredError("OP_INVALID", "alterColumn change must carry `type` or `nullable`");
}

export function alterColumnType(table, column, type, opts = {}) {
  requireString(table, "alterColumnType(table, …)");
  requireString(column, "alterColumnType(table, column, …)");
  return push(
    compact({
      op: "alterColumnType",
      table,
      column,
      type: colTypeOf(type),
      using: resolveExpr(opts.using),
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

export function alterColumnNullability(table, column, nullable, opts = {}) {
  requireString(table, "alterColumnNullability(table, …)");
  requireString(column, "alterColumnNullability(table, column, …)");
  if (typeof nullable !== "boolean") {
    throw structuredError("OP_INVALID", "alterColumnNullability nullable must be a boolean");
  }
  return push(
    compact({
      op: "alterColumnNullability",
      table,
      column,
      nullable,
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

// ── DDL: constraints / indexes — every adder is (table, spec); `name` in spec ──

/** Build an `IrConstraint` of kind `fk` from a `{ columns, references:{ table,
 *  columns }, onDelete?, onUpdate?, name? }` spec — order-independent named
 *  fields (NOT transposable positionals). `onDelete`/`onUpdate` are not part of
 *  the frozen IrConstraintKind::fk shape, so they are dropped here (a follow-up
 *  PR can carry FK actions); the columns/referencesTable/referencesColumns are
 *  the frozen wire fields. */
function fkConstraintFromSpec(spec) {
  if (!spec || typeof spec !== "object" || !spec.references) {
    throw structuredError("OP_INVALID", "addForeignKey spec needs { columns, references:{ table, columns } }");
  }
  return compact({
    name: spec.name,
    kind: {
      kind: "fk",
      columns: spec.columns,
      referencesTable: spec.references.table,
      referencesColumns: spec.references.columns,
    },
  });
}

export function addForeignKey(table, spec, opts = {}) {
  requireString(table, "addForeignKey(table, …)");
  return push(
    compact({
      op: "addConstraint",
      table,
      constraint: fkConstraintFromSpec(spec),
      schema: opts.schema,
      existenceGuard: ifNotExistsGuard(opts.ifNotExists),
    }),
  );
}

export function addUnique(table, spec, opts = {}) {
  requireString(table, "addUnique(table, …)");
  if (!spec || !Array.isArray(spec.columns)) {
    throw structuredError("OP_INVALID", "addUnique spec needs { columns: string[], name? }");
  }
  return push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name: spec.name, kind: { kind: "unique", columns: spec.columns } }),
      schema: opts.schema,
      existenceGuard: ifNotExistsGuard(opts.ifNotExists),
    }),
  );
}

/** Legacy: the PR1 `addConstraint(table, constraint)` form, taking a pre-built
 *  `IrConstraint` wire object directly. Retained for the existing corpus; new
 *  authoring uses the typed `addForeignKey`/`addUnique`/`addCheck` adders. */
export function addConstraint(table, constraint, opts = {}) {
  requireString(table, "addConstraint(table, …)");
  return push(
    compact({
      op: "addConstraint",
      table,
      constraint,
      schema: opts.schema,
      existenceGuard: ifNotExistsGuard(opts.ifNotExists),
    }),
  );
}

export function addCheck(table, spec, opts = {}) {
  requireString(table, "addCheck(table, …)");
  if (!spec || spec.expr === undefined) {
    throw structuredError("OP_INVALID", "addCheck spec needs { expr: (c) => Expr, name? }");
  }
  return push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name: spec.name, kind: { kind: "check", expr: resolveExpr(spec.expr) } }),
      schema: opts.schema,
      existenceGuard: ifNotExistsGuard(opts.ifNotExists),
    }),
  );
}

/**
 * `dropConstraint(table, spec)` — spec is `{ name, type?, ifExists? }` (the §3.2
 * form), OR the legacy bare `name` string. The frozen `dropConstraint` op
 * carries only `{ table, name }` (the `type`/`ifExists` hints are validator
 * niceties not in the frozen wire shape, so they are not recorded).
 */
export function dropConstraint(table, spec, opts = {}) {
  requireString(table, "dropConstraint(table, …)");
  const name = typeof spec === "string" ? spec : spec && spec.name;
  requireString(name, "dropConstraint name");
  // **PR10** — `ifExists` may ride on the spec object (the §3.2 form) or the
  // explicit opts bag; the schema qualifier rides on opts.
  const ifExists = (spec && typeof spec === "object" && spec.ifExists) || opts.ifExists;
  return push(
    compact({
      op: "dropConstraint",
      table,
      name,
      schema: opts.schema,
      existenceGuard: ifExistsGuard(ifExists),
    }),
  );
}

/**
 * `createIndex(table, spec)` — spec `{ columns, name?, unique?, using?, where?,
 * concurrently? }` (the §3.2 form). `where` is an `ExprFn`. The legacy
 * `createIndex(table, columns, opts)` positional form is also accepted.
 */
export function createIndex(table, specOrColumns, legacyOpts) {
  requireString(table, "createIndex(table, …)");
  let spec;
  if (Array.isArray(specOrColumns)) {
    // Legacy positional: (table, columns[], opts).
    spec = { columns: specOrColumns, ...(legacyOpts || {}) };
  } else {
    spec = specOrColumns || {};
  }
  if (!Array.isArray(spec.columns)) {
    throw structuredError("OP_INVALID", "createIndex spec needs { columns: string[], … }");
  }
  return push(
    compact({
      op: "createIndex",
      table,
      columns: spec.columns,
      name: spec.name,
      unique: spec.unique,
      using: spec.using,
      where: resolveExpr(spec.where),
      concurrently: spec.concurrently,
      schema: spec.schema,
      existenceGuard: ifNotExistsGuard(spec.ifNotExists),
    }),
  );
}

export function dropIndex(name, opts = {}) {
  requireString(name, "dropIndex(name, …)");
  return push(
    compact({
      op: "dropIndex",
      name,
      table: opts.table,
      unique: opts.unique,
      concurrently: opts.concurrently,
      schema: opts.schema,
      existenceGuard: ifExistsGuard(opts.ifExists),
    }),
  );
}

// ── DML: insert / update / del / backfill (verb(table, { … })) ──

/**
 * `insert(table, { rows, onConflict? })` — the §3.2 form, with `rows` a row
 * object `{ col: scalar }` or an array of them. The legacy positional form
 * `insert(table, columns[], rows[][], { onConflict })` is also accepted (rows as
 * positional scalar arrays).
 *
 * The wire op carries `{ columns: string[], rows: scalar[][] }`; the fluent
 * row-object form is normalized into the columns + positional-rows shape (column
 * order = first row's key order, deterministic).
 */
export function insert(table, arg2, arg3, arg4) {
  requireString(table, "insert(table, …)");

  // Legacy positional: (table, columns[], rows[][], opts).
  if (Array.isArray(arg2)) {
    return push(
      compact({
        op: "insert",
        table,
        columns: arg2,
        rows: normalizeRows(arg3),
        onConflict: normalizeOnConflict(arg4 && arg4.onConflict),
        schema: arg4 && arg4.schema,
      }),
    );
  }

  // Fluent: (table, { rows, onConflict? }).
  const args = arg2 || {};
  let rows = args.rows;
  if (rows === undefined) {
    throw structuredError("OP_INVALID", "insert(table, { rows }): rows is required");
  }
  if (!Array.isArray(rows)) rows = [rows];

  // Already in positional `scalar[][]` form? (a legacy caller passing rows as
  // arrays inside the bag). Otherwise normalize the row OBJECTS into columns +
  // positional rows, with column order from the first row's keys.
  if (rows.length > 0 && Array.isArray(rows[0])) {
    if (!args.columns) {
      throw structuredError("OP_INVALID", "insert rows given as arrays needs a `columns` list");
    }
    return push(
      compact({
        op: "insert",
        table,
        columns: args.columns,
        rows: normalizeRows(rows),
        onConflict: normalizeOnConflict(args.onConflict),
        schema: args.schema,
      }),
    );
  }

  const columns = rows.length > 0 ? Object.keys(rows[0]) : args.columns || [];
  const positional = rows.map((r) =>
    columns.map((col) => (Object.prototype.hasOwnProperty.call(r, col) ? toIrScalar(r[col]) : null)),
  );
  return push(
    compact({
      op: "insert",
      table,
      columns,
      rows: positional,
      onConflict: normalizeOnConflict(args.onConflict),
      schema: args.schema,
    }),
  );
}

/** Normalize each positional row cell through the `IrScalar` wire carrier (a
 *  bigint/Uint8Array author value → its `{decimal}`/`{bytes}` carrier). */
function normalizeRows(rows) {
  if (!Array.isArray(rows)) return rows;
  return rows.map((row) => (Array.isArray(row) ? row.map(toIrScalar) : row));
}

/** Normalize an `onConflict.doUpdate` `column → scalar` map through the IrScalar
 *  carrier so a bigint/Uint8Array assignment matches the Rust `IrOnConflict`
 *  `BTreeMap<String, IrScalar>` shape (§2.4.1). */
function normalizeOnConflict(oc) {
  if (oc === undefined || oc === null) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns };
  const doUpdate = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrScalar(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate };
}

/**
 * `update(table, { set, where? })` — `set` is `{ col: ExprFn }`, `where` an
 * `ExprFn`. The legacy positional `update(table, setMap, { where, batch })` is
 * also accepted.
 */
export function update(table, arg2, arg3) {
  requireString(table, "update(table, …)");
  // Disambiguate: the §3.2 form is `{ set, where? }` (a `set` key); the legacy
  // form is `(table, setMap, { where, batch })`.
  let set;
  let where;
  let batch;
  let schema;
  if (arg2 && typeof arg2 === "object" && arg2.set !== undefined) {
    set = arg2.set;
    where = arg2.where;
    batch = arg2.batch;
    schema = arg2.schema;
  } else {
    set = arg2;
    where = arg3 && arg3.where;
    batch = arg3 && arg3.batch;
    schema = arg3 && arg3.schema;
  }
  return push(
    compact({
      op: "update",
      table,
      set: resolveSet(set),
      where: resolveExpr(where),
      batch,
      schema,
    }),
  );
}

/**
 * `del(table, { where, limit? })` — `where` is MANDATORY (no unfiltered delete).
 * `del` not `delete` (the JS reserved word); the recorded discriminant is the
 * full `"delete"` (the wire-tag mapping pinned in the ADR). The legacy positional
 * `del(table, whereExpr, { limit })` is also accepted.
 */
export function del(table, arg2, arg3) {
  requireString(table, "del(table, …)");
  let where;
  let limit;
  let schema;
  if (arg2 && typeof arg2 === "object" && !(arg2 instanceof ExprChain) && arg2.where !== undefined) {
    where = arg2.where;
    limit = arg2.limit;
    schema = arg2.schema;
  } else {
    where = arg2;
    limit = arg3 && arg3.limit;
    schema = arg3 && arg3.schema;
  }
  if (where === undefined || where === null) {
    throw structuredError("OP_INVALID", "del(table, { where }): where is mandatory (no unfiltered delete)");
  }
  return push(compact({ op: "delete", table, where: resolveExpr(where), limit, schema }));
}

/**
 * `backfill(table, { set, where?, cursorColumn?, batchSize? })` — the §3.2 form.
 * `cursorColumn` defaults to `"id"` (the single-column PK convention), `batchSize`
 * to an engine default (1000) when omitted. The legacy positional
 * `backfill(table, cursorColumn, batchSize, set, name, { filter })` is also
 * accepted. The predicate keyword is `where` everywhere (the wire field is
 * `filter`).
 */
const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

export function backfill(table, arg2, arg3, arg4, arg5, arg6) {
  requireString(table, "backfill(table, …)");

  // Legacy positional: (table, cursorColumn, batchSize, set, name, { filter }).
  if (typeof arg2 === "string") {
    return push(
      compact({
        op: "backfill",
        table,
        cursorColumn: arg2,
        batchSize: arg3,
        set: resolveSet(arg4),
        filter: resolveExpr(arg6 && arg6.filter),
        name: arg5,
        schema: arg6 && arg6.schema,
      }),
    );
  }

  // Fluent: (table, { set, where?, cursorColumn?, batchSize?, name? }).
  const args = arg2 || {};
  if (args.set === undefined) {
    throw structuredError("OP_INVALID", "backfill(table, { set }): set is required");
  }
  const cursorColumn = args.cursorColumn || DEFAULT_BACKFILL_CURSOR;
  const batchSize = args.batchSize !== undefined ? args.batchSize : DEFAULT_BACKFILL_BATCH;
  // The journaled progress key defaults to a stable per-table label.
  const name = args.name || `backfill_${table}`;
  return push(
    compact({
      op: "backfill",
      table,
      cursorColumn,
      batchSize,
      set: resolveSet(args.set),
      filter: resolveExpr(args.where),
      name,
      schema: args.schema,
    }),
  );
}

/**
 * `batchAlterTable(table, build)` — the SQLite-safe rebuild (Alembic
 * `batch_alter_table` analog, §3.2). `build` receives a scoped subset of the
 * column/constraint op-functions pre-bound to `table`, recording into the same
 * ambient recorder.
 */
export function batchAlterTable(table, build) {
  requireString(table, "batchAlterTable(table, …)");
  if (typeof build !== "function") {
    throw structuredError("OP_INVALID", "batchAlterTable(table, build): build must be a callback");
  }
  const scoped = {
    addColumn: (name, type, opts) => addColumn(table, name, type, opts),
    dropColumn: (name, opts) => dropColumn(table, name, opts),
    renameColumn: (from, to, type) => renameColumn(table, from, to, type),
    alterColumn: (name, change) => alterColumn(table, name, change),
    addForeignKey: (spec) => addForeignKey(table, spec),
    addCheck: (spec) => addCheck(table, spec),
  };
  build(scoped);
}

// ===========================================================================
// (D) The eager fluent `table()` facade (PR11). The byte-for-byte twin of
// `sdks/migrate/src/ops.ts`'s `table()`: a recorder-bound handle whose methods
// mirror the flat ops scoped to one table, recording EAGERLY (the call IS the
// recording — no terminal to forget). PURE SUGAR: every method DELEGATES to the
// SAME flat op-function above (single source of truth — it never re-implements op
// construction), exactly as `batchAlterTable` does. The `{ schema }` from
// `table()` is the DEFAULT injected into every recorded op; a per-method `schema`
// OVERRIDES it (by key presence — an absent/`undefined` per-call schema keeps the
// table default). A `table()`-authored migration records the IDENTICAL op list as
// the equivalent flat-op migration (byte-identical IR).
// ===========================================================================

/** Per-method-wins-over-table-default schema precedence (PR11): a per-call `schema`
 *  overrides the table default only when present + defined; an omitted/`undefined`
 *  per-call schema keeps the table default. */
function pickSchema(perCall, dflt) {
  if (perCall && perCall.schema !== undefined) return perCall.schema;
  return dflt;
}

export function table(name, opts = {}) {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;
  return {
    // DDL: this table — delegates to the flat `createTable(name, columns, build?,
    // opts)` (the twin now accepts the 4-arg build+opts form, mirroring ops.ts).
    create(columns, build, createOpts = {}) {
      createTable(name, columns, build, {
        schema: pickSchema(createOpts, dflt),
        ifNotExists: createOpts.ifNotExists,
      });
    },
    drop(dropOpts = {}) {
      dropTable(name, {
        schema: pickSchema(dropOpts, dflt),
        ifExists: dropOpts.ifExists,
        cascade: dropOpts.cascade,
      });
    },

    // DDL: columns
    addColumn(col, type, colOpts = {}) {
      addColumn(name, col, type, {
        schema: pickSchema(colOpts, dflt),
        ifNotExists: colOpts.ifNotExists,
      });
    },
    dropColumn(col, colOpts = {}) {
      dropColumn(name, col, {
        schema: pickSchema(colOpts, dflt),
        ifExists: colOpts.ifExists,
      });
    },
    renameColumn(from, to, type, renameOpts = {}) {
      renameColumn(name, from, to, type, {
        schema: pickSchema(renameOpts, dflt),
        ifExists: renameOpts.ifExists,
      });
    },
    alterColumn(col, change, alterOpts = {}) {
      alterColumn(name, col, change, {
        schema: pickSchema(alterOpts, dflt),
        ifExists: alterOpts.ifExists,
      });
    },

    // DDL: constraints / indexes
    addForeignKey(spec, fkOpts = {}) {
      addForeignKey(name, spec, {
        schema: pickSchema(fkOpts, dflt),
        ifNotExists: fkOpts.ifNotExists,
      });
    },
    addUnique(spec, uqOpts = {}) {
      addUnique(name, spec, {
        schema: pickSchema(uqOpts, dflt),
        ifNotExists: uqOpts.ifNotExists,
      });
    },
    addCheck(spec, ckOpts = {}) {
      addCheck(name, spec, {
        schema: pickSchema(ckOpts, dflt),
        ifNotExists: ckOpts.ifNotExists,
      });
    },
    dropConstraint(spec, dcOpts = {}) {
      dropConstraint(name, spec, {
        schema: pickSchema(dcOpts, dflt),
        ifExists: dcOpts.ifExists,
      });
    },
    createIndex(spec) {
      createIndex(name, { ...spec, schema: pickSchema(spec, dflt) });
    },
    dropIndex(idxName, idxOpts = {}) {
      dropIndex(idxName, {
        table: name,
        schema: pickSchema(idxOpts, dflt),
        ifExists: idxOpts.ifExists,
        unique: idxOpts.unique,
        concurrently: idxOpts.concurrently,
      });
    },

    // DML — schema rides on the args object; no existence guard.
    insert(args) {
      insert(name, { ...args, schema: pickSchema(args, dflt) });
    },
    update(args) {
      update(name, { ...args, schema: pickSchema(args, dflt) });
    },
    del(args) {
      del(name, { ...args, schema: pickSchema(args, dflt) });
    },
    backfill(args) {
      backfill(name, { ...args, schema: pickSchema(args, dflt) });
    },
  };
}

// ===========================================================================
// LEGACY: the PR1 `e.*` closed-AST node helper. NOT the public §3.3.1 surface
// (the fluent `(c) => Expr` builder above is), but retained so the PR1 golden
// corpus + `split_part_lint.rs` (which author Expr nodes directly via `e.*`)
// keep recording the IDENTICAL wire op. New authoring should use the `(c) => Expr`
// callback; `e.*` is the structural node form the recorder ultimately captures.
// ===========================================================================

export const e = {
  col: (name) => ({ node: "colRef", name }),
  lit: (value) => ({ node: "literal", value }),
  binOp: (op, lhs, rhs) => ({ node: "binOp", op, lhs, rhs }),
  unaryOp: (op, operand) => ({ node: "unaryOp", op, operand }),
  fnCall: (fn, args) => ({ node: "fnCall", fn, args }),
  fnSynth: (fn, args) => ({ node: "fnSynth", fn, args }),
  cast: (operand, target) => ({ node: "cast", operand, target }),
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return {
      node: "fnSynth",
      fn: "splitPart",
      args: [normalizeExprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    };
  },
};

// ===========================================================================
// (C) Determinism lint (§4.3). Flag the JS nondeterminism accessors — the
// current-wall-clock accessor (`Date.now()`), the RNG accessor (`Math.random()`),
// the UUID accessor (`crypto.randomUUID()`), and the `new Date()` clock
// constructor — when they appear in an op argument, steering authors to the
// DB-evaluated `c.fn.now()` / `c.fn.genRandomUuid()` (`FnSynth`) scalars.
//
// The recorder captures only the RESULT of a JS expression, so a `Date.now()`
// already collapsed to a number is indistinguishable from a hand-typed literal at
// record time. This lint is therefore a BEST-EFFORT, AST-free SOURCE scan over
// the migration text (the §4.3 "ESLint/DSL lint … syntactically appearing inside
// an op-function argument" mechanism). The authoritative determinism guarantee is
// the build-once committed artifact (§5.1); this is the pre-commit catch.
// ===========================================================================

/** The nondeterminism accessors the §4.3 lint flags, with the steer to the
 *  DB-evaluated synth scalar. */
const NONDETERMINISM_PATTERNS = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "c.fn.genRandomUuid() (for an id) or a DB-evaluated value" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "c.fn.now()" },
];

/**
 * Lint a migration's SOURCE TEXT for the §4.3 nondeterminism accessors. Returns
 * an array of `{ code, accessor, suggested_fix, reason }` findings (empty ⇒
 * clean). Exposed so the build/CLI/record path can surface findings on changed
 * migrations before commit (§4.3 mechanism (a)).
 *
 * SCOPE — intentional coarse whole-source scan. §4.3 specifies "syntactically
 * appearing inside an op-function argument", but this lint is a deliberate
 * fail-SAFE whole-source regex scan: it OVER-flags (a clock accessor in a comment
 * or a non-op helper trips it) and NEVER under-flags. That is the chosen contract,
 * not a bug — the build-once committed artifact (§5.1) already neutralizes
 * post-deploy non-determinism, so the lint's only job is a best-effort pre-commit
 * STEER (§8.8), where a false positive is cheap (rephrase) and a false negative
 * (a baked build-time value slipping through) is the real hazard. The record path
 * (`record_migration_to_ir_with_warnings`) surfaces these as WARNINGS, never a hard
 * reject. Narrowing to true op-arg spans is a possible future precision
 * improvement; the over-flag behavior is pinned by test.
 */
export function lintDeterminism(source) {
  if (typeof source !== "string") return [];
  const findings = [];
  for (const { re, name, steer } of NONDETERMINISM_PATTERNS) {
    if (re.test(source)) {
      findings.push({
        code: "NONDETERMINISTIC_OP_ARG",
        accessor: name,
        suggested_fix: `replace ${name} with the DB-evaluated ${steer}`,
        reason:
          `${name} bakes a build-time value into the migration artifact; for a value that ` +
          "must be computed at apply time use the structured FnSynth scalar (§4.3)",
      });
    }
  }
  return findings;
}

// ---------------------------------------------------------------------------
// `c.fn.splitPart` grammar lint (§9) — the dialect-NEUTRAL subset broken on BOTH
// backends (a non-string/empty delimiter, a non-positive-int n). A violation
// throws a structured EXPR_NOT_PORTABLE error (the §8.8 machine-readable
// rejection the AI loop self-corrects on). The portability ENVELOPE (single-ASCII
// delimiter, 1<=n<=8) is NOT checked here — it is dialect-gated and deferred to
// the Rust validator (admit on PG via dialect_scope=PgOnly, reject on SQLite).
// ---------------------------------------------------------------------------

function splitPartGrammarLint(delim, n) {
  const fail = (reason) => {
    throw structuredError("EXPR_NOT_PORTABLE", reason, {
      suggested_fix:
        "pass a non-empty string-literal delimiter and a positive-integer n; to target " +
        "SQLite too, stay in-envelope (single-ASCII delimiter, 1<=n<=8) — a multi-char/" +
        "non-ASCII delimiter or n>8 renders only on Postgres (dialect_scope=PgOnly)",
    });
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

/** Coerce a legacy `e.*` arg: a bare string is a ColRef shorthand; an already-
 *  built `{node:…}` passes through; anything else is a literal. */
function normalizeExprArg(arg) {
  if (arg && typeof arg === "object" && typeof arg.node === "string") {
    return arg;
  }
  if (typeof arg === "string") {
    return { node: "colRef", name: arg };
  }
  return { node: "literal", value: arg };
}
