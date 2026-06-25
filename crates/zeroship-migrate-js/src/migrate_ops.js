// The `@zeroship/migrate` op-builder DSL — the engine V8 recorder, the LOCK-STEP
// twin of `sdks/migrate/src/ops.ts` (design `2026-06-25-op-dsl-fluent-redesign.md`).
// The Rust runtime `include_str!`s this into V8 to turn a creator's
// `import { table, t } from "@zeroship/migrate"` migration into the canonical
// `.ir.json` the lean engine deserializes.
//
// FLUENT-ONLY: `table()` is the SOLE public entry. The flat op-functions and the
// `e.*` node helper are GONE (pre-launch, no back-compat). Their op-construction
// logic survives as the internal `recordX` helpers the handle delegates to, so the
// emitted wire ops are byte-identical to the pre-redesign flat surface — EXCEPT
// the C1 FK-actions delta. The `.ir.json` shape is frozen and dialect-neutral; the
// golden corpus (`tests/op_fixtures`) + the `Checksum::of_ir` round-trip + the
// variant-exhaustiveness gate are the contract.
//
// CONTRACT: each terminal RECORDS one canonical op object onto the module-local
// recording buffer, synchronously. A migration authors inside `up()`/`down()`; the
// adapter drains the buffer per phase and emits the `.ir.json` envelope. Authoring
// OUTSIDE an active recorder throws a structured `OP_OUTSIDE_RECORDER`. A selector
// that is never terminated throws `SELECTOR_NOT_TERMINATED` at DRAIN (§5).
//
// CHECKSUM: the JS side NEVER computes the checksum; the Rust engine is the single
// `Checksum::of_ir` authority. JS emits ops; Rust folds.
//
// WIRE SHAPE: the op-region fields are camelCase (`ifExists`, `cursorColumn`,
// `batchSize`, `referencesTable`, `onDelete`), matching `op-ir.schema.json`. An
// absent optional is OMITTED (never `field: undefined`/`null`) so the JCS image
// matches the Rust `skip_serializing_if` omitted-key image.

// ---------------------------------------------------------------------------
// The ambient per-migration recorder (§3.1 / §5). The adapter installs a fresh
// recorder before each phase. Authoring OUTSIDE an active recorder is a structured
// error. A handed-out selector that is never terminated is a structured error at
// DRAIN.
// ---------------------------------------------------------------------------

let __active = null;

/** Structured error helper — mirrors the machine-readable envelope. */
function structuredError(code, message, extra) {
  const err = new Error(message);
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

/** Begin a fresh recording buffer (called by the adapter before a phase). */
export function __begin() {
  __active = { ops: [], pending: new Map(), nextSelectorId: 0 };
}

/** Drain + return the recorded op list, clearing the active recorder. At DRAIN
 *  (not eagerly — so a var-held selector terminated on a later line is fine),
 *  any selector handed out but never terminated is a hard SELECTOR_NOT_TERMINATED
 *  error. Returns `[]` if no recorder is active. */
export function __drain() {
  if (__active === null) return [];
  const rec = __active;
  __active = null;
  for (const sel of rec.pending.values()) {
    if (!sel.terminated) {
      throw structuredError(
        "SELECTOR_NOT_TERMINATED",
        `selector .${sel.selector}(${JSON.stringify(sel.name)}) was never terminated; ` +
          "a selector records nothing until its terminal (.add/.drop/.rename/.alter) is called",
        {
          selector: sel.selector,
          name: sel.name,
          suggested_fix: `call a terminal on .${sel.selector}(${JSON.stringify(sel.name)}) ` +
            "(e.g. .add({…})) or remove the selector",
        },
      );
    }
  }
  return rec.ops;
}

function recorder() {
  if (__active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      "op authoring called outside an active migration recorder; " +
        "the table() handle may only be used synchronously inside up()/down() " +
        "(not at module top level or after the phase returns)",
      {
        suggested_fix:
          "move the table()/selector calls inside the migration's up()/down() body",
      },
    );
  }
  return __active;
}

function push(op) {
  recorder().ops.push(op);
  return op;
}

/** Register a handed-out selector; returns its id (used at terminate). */
function registerSelector(selector, name) {
  const rec = recorder();
  const id = rec.nextSelectorId++;
  rec.pending.set(id, { selector, name, terminated: false });
  return id;
}

/** Mark a selector terminated; double-terminate is a structured error (§5). */
function terminateSelector(id) {
  const rec = recorder();
  const sel = rec.pending.get(id);
  if (sel === undefined) return;
  if (sel.terminated) {
    throw structuredError(
      "SELECTOR_ALREADY_TERMINATED",
      `selector .${sel.selector}(${JSON.stringify(sel.name)}) was terminated twice; ` +
        "each selector records exactly one op",
      { selector: sel.selector, name: sel.name },
    );
  }
  sel.terminated = true;
}

/** Drop keys whose value is `undefined` so an absent optional is OMITTED on the
 *  wire (never `"k":null`) — the cross-impl determinism contract. */
function compact(obj) {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function requireString(v, what) {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

// ===========================================================================
// (B) The IMMUTABLE chainable `t.*` column-type lexicon (§4). NULLABLE BY
// DEFAULT; `.notNull()` / `.default(x)` / `.ref(target)` / `.primaryKey()` /
// `.unique()` opt in. Each modifier returns a FRESH ColumnDef (no receiver
// mutation), so a hoisted type var is safe to reuse across columns. The
// options-bag overload and the `string`/`int` aliases are REMOVED (§7).
// ===========================================================================

class ColumnDef {
  /** @param {object} colType the dialect-neutral ColType wire value (§4).
   *  @param {object} [fields] nullable/default/primaryKey/unique overrides. */
  constructor(colType, fields) {
    this._type = colType;
    this._nullable = fields && fields.nullable !== undefined ? fields.nullable : true;
    this._default = fields ? fields.default : undefined;
    this._primaryKey = fields && fields.primaryKey !== undefined ? fields.primaryKey : false;
    this._unique = fields && fields.unique !== undefined ? fields.unique : false;
  }

  /** Clone with the named fields overridden — the basis of immutability (§4). */
  _with(over) {
    return new ColumnDef(over.type !== undefined ? over.type : this._type, {
      nullable: over.nullable !== undefined ? over.nullable : this._nullable,
      default: "default" in over ? over.default : this._default,
      primaryKey: over.primaryKey !== undefined ? over.primaryKey : this._primaryKey,
      unique: over.unique !== undefined ? over.unique : this._unique,
    });
  }

  notNull() {
    return this._with({ nullable: false });
  }

  /** `.default(value | { fn: "now" | "genRandomUuid" })` → a structured IrDefault
   *  (typed literal OR nullary synth scalar) — NEVER raw SQL (property A). */
  default(value) {
    return this._with({ default: toIrDefault(value) });
  }

  /** Re-target a column as a foreign-key reference. */
  ref(targetTable) {
    requireString(targetTable, "t.*.ref(target)");
    return this._with({ type: { ref: { references: targetTable } } });
  }

  primaryKey() {
    return this._with({ primaryKey: true, nullable: false });
  }

  unique() {
    return this._with({ unique: true });
  }

  /** Reduce to an `IrColumn` (the `createTable` columns[] shape). `name` is the
   *  map key. `nullable`/`default`/`unique` omitted when at their defaults. */
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

/** Marker the helpers use to tell a fluent `ColumnDef` from a bare ColType. */
function isColumnDef(x) {
  return x instanceof ColumnDef;
}

/** Base64-encode raw bytes (the `IrScalar::Bytes` wire carrier) without a Node
 *  `Buffer` — `btoa` is a WHATWG global present in the V8 record host + Node. */
function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/** Normalize a JS scalar into the closed `IrScalar` WIRE carrier (§3.5):
 *   - a JS `bigint` → `{ decimal: "<v>" }`;
 *   - a `Uint8Array` → `{ bytes: "<base64>" }`;
 *   - everything else passes through verbatim. */
function toIrScalar(value) {
  if (typeof value === "bigint") return { decimal: value.toString() };
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  return value;
}

/** Coerce a `.default(value)` arg into the closed `IrDefault` carrier:
 *   - `{ fn: "now" | "genRandomUuid" }` → a nullary synth default;
 *   - any other typed scalar → a `{ literal: { value } }` literal default. */
function toIrDefault(value) {
  if (value && typeof value === "object" && typeof value.fn === "string") {
    return { fn: { fn: value.fn } };
  }
  return { literal: { value: toIrScalar(value) } };
}

/** The immutable fluent column-type lexicon (§4). Canonical names only. */
export const t = {
  /** A conventional primary-key id: a non-null UUID PK defaulting to a DB-evaluated
   *  `gen_random_uuid()` (the structured FnSynth default, never a frozen literal). */
  id: () => new ColumnDef("uuid").primaryKey().default({ fn: "genRandomUuid" }),
  text: () => new ColumnDef("text"),
  /** Fixed-precision decimal. Defaults to (38, 9). */
  numeric: (precision = 38, scale = 9) => new ColumnDef({ decimal: { precision, scale } }),
  timestamp: () => new ColumnDef("timestamp"),
  uuid: () => new ColumnDef("uuid"),
  bytes: () => new ColumnDef("bytea"),
  boolean: () => new ColumnDef("bool"),
  json: () => new ColumnDef("json"),
  /** A foreign-key reference column carrying a plain-string target table name. */
  ref: (targetTable) => {
    requireString(targetTable, "t.ref(target)");
    return new ColumnDef({ ref: { references: targetTable } });
  },
  vector: (n) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    return new ColumnDef({ vector: { vector: n } });
  },
  geoPoint: () => new ColumnDef("geoPoint"),
  /** 32-bit signed integer (canonical; the `int` alias is removed, §7). */
  integer: () => new ColumnDef("int"),
  bigInt: () => new ColumnDef("bigInt"),
  float: () => new ColumnDef("float"),
  /** An application-level encrypted column wrapping an inner `t.*` type. */
  encrypted: (arg) => {
    const inner = arg && arg.of !== undefined ? arg.of : arg;
    const innerType = isColumnDef(inner) ? inner._type : inner;
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return new ColumnDef({ encrypted: { of: innerType } });
  },
};

/** Resolve a column-type argument to its ColType wire value (a fluent ColumnDef
 *  or a bare ColType string/object). */
function colTypeOf(typeArg) {
  if (isColumnDef(typeArg)) return typeArg._type;
  return typeArg;
}

// ===========================================================================
// (B continued) The single-handle fluent `(c) => Expr` builder (§3.6). `c` is
// BOTH a column-accessor function (`c("name")` → a chainable ColRef) and the
// `c.fn.*` scalar-function namespace. `coalesce`/`concatWs` live ONLY on `c.fn`
// (§7 dedup). The chain auto-wraps bare JS values to `Literal`.
// ===========================================================================

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
   *  receiver + every part. For NULL-skipping joins use `c.fn.concatWs` (§3.6). */
  concat(...parts) {
    let acc = this.__node;
    for (const p of parts) {
      acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    }
    return chain(acc);
  }

  // ── null/bool tests ──
  isNull() { return chain({ node: "unaryOp", op: "isNull", operand: this.__node }); }
  isNotNull() { return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node }); }
  isTrue() { return chain({ node: "unaryOp", op: "isTrue", operand: this.__node }); }
  isFalse() { return chain({ node: "unaryOp", op: "isFalse", operand: this.__node }); }

  // ── cast ──
  /** `.cast("integer" | "text" | "real" | "boolean" | "blob")` — the closed
   *  portable target set (§3.6). */
  cast(target) {
    return chain({ node: "cast", operand: this.__node, target });
  }
}

/** Build the single fluent handle `c`: a column-accessor function carrying the
 *  `c.fn.*` namespace. `c("name")` → a chainable ColRef (a plain-string name). */
function makeBuilder() {
  const c = (name) => {
    requireString(name, 'c("name")');
    return chain({ node: "colRef", name });
  };
  c.fn = cFn; // the scalar-function namespace (§3.6)
  return c;
}

/** Resolve an expression slot: an `ExprFn` callback `(c) => Expr`, a chainable
 *  `ExprChain`, or a raw closed-AST node object. Returns the closed-AST node. */
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
    "expression slot must be a (c) => Expr callback or a built expression",
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
// (B continued) `c.fn.*` — the scalar-function namespace (§3.6). Reached off the
// single builder handle (no importable `fn`). `coalesce`/`concatWs` live ONLY
// here (§7 dedup). Each member builds exactly one closed-AST node.
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
   *  join helper (§3.6). `sep` is a literal. */
  concatWs: (sep, ...parts) =>
    chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),

  /** The searched `CASE` form (`c.fn.case([[cond, val], …], elseVal)`). */
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
   *  literal; `n` a positive integer literal. */
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },

  /** DB-evaluated apply-time scalars (the structured replacement for a frozen
   *  `Date.now()` / UUID literal). Render to `now()` / `gen_random_uuid()`. */
  now: () => chain({ node: "fnSynth", fn: "now", args: [] }),
  genRandomUuid: () => chain({ node: "fnSynth", fn: "genRandomUuid", args: [] }),
};

// ===========================================================================
// (C) Existence-guard token mappers. The create/add family takes `ifNotExists`;
// the drop/rename/alter family takes `ifExists`. Engine-synthesized via a catalog
// probe — NOT a native `IF [NOT] EXISTS` clause.
// ===========================================================================

function ifNotExistsGuard(v) {
  return v ? "ifNotExists" : undefined;
}
function ifExistsGuard(v) {
  return v ? "ifExists" : undefined;
}

// ===========================================================================
// (D) The internal op-construction helpers (the single source of truth). These
// build + push the EXACT canonical op object the Rust closed `Op` enum /
// `op-ir.schema.json` deserialize (byte-identical to the pre-redesign flat
// surface except the C1 FK-actions delta). Only the fluent `table()` handle calls
// them.
// ===========================================================================

function recordCreateTable(name, args) {
  const cols = [];
  const constraints = [];
  const indexes = [];
  const pkCols = [];

  const columns = args.columns || {};
  for (const colName of Object.keys(columns)) {
    const def = columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError(
        "OP_INVALID",
        `create column "${colName}" must be a t.* ColumnDef (got ${typeof def})`,
      );
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  // A composite `primaryKey: [...]` wins over per-column `.primaryKey()` hoists.
  const pk = Array.isArray(args.primaryKey) && args.primaryKey.length > 0 ? args.primaryKey : pkCols;
  if (pk.length > 0) constraints.push({ kind: { kind: "pk", columns: pk } });

  for (const uq of args.uniques || []) {
    constraints.push(compact({ name: uq.name, kind: { kind: "unique", columns: uq.columns } }));
  }
  for (const ck of args.checks || []) {
    constraints.push(compact({ name: ck.name, kind: { kind: "check", expr: resolveExpr(ck.expr) } }));
  }
  for (const fkSpec of args.foreignKeys || []) {
    constraints.push(
      fkConstraintFromSpec({
        name: fkSpec.name,
        columns: fkSpec.columns,
        references: fkSpec.references,
        onDelete: fkSpec.onDelete,
        onUpdate: fkSpec.onUpdate,
      }),
    );
  }
  for (const idx of args.indexes || []) {
    indexes.push(
      compact({
        name: idx.name,
        columns: idx.columns,
        unique: idx.unique,
        using: idx.using,
        where: resolveExpr(idx.where),
      }),
    );
  }

  push(
    compact({
      op: "createTable",
      name,
      columns: cols,
      constraints: constraints.length ? constraints : undefined,
      indexes: indexes.length ? indexes : undefined,
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropTable(table, args) {
  push(
    compact({
      op: "dropTable",
      table,
      cascade: args.cascade,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordAddColumn(table, column, type, args) {
  push(
    compact({
      op: "addColumn",
      table,
      column,
      ...type.__toAddColumnTail(),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
  // C2 — `.column(x).add({ type: t.text().unique() })` honors `.unique()`: an
  // ADD COLUMN has no inline UNIQUE, so it lowers to a separate ADD CONSTRAINT.
  // Likewise `.primaryKey()` hoists a pk add. A PRIMARY KEY already IMPLIES
  // uniqueness, so when BOTH are set the follow-on UNIQUE is redundant DDL —
  // suppress it (lock-step with the TS surface + the differ, which never emits a
  // separate UNIQUE for the PK column). Only the pk add is recorded.
  if (type._unique && !type._primaryKey) {
    push(
      compact({
        op: "addConstraint",
        table,
        constraint: { kind: { kind: "unique", columns: [column] } },
        schema: args.schema,
        existenceGuard: ifNotExistsGuard(args.ifNotExists),
      }),
    );
  }
  if (type._primaryKey) {
    push(
      compact({
        op: "addConstraint",
        table,
        constraint: { kind: { kind: "pk", columns: [column] } },
        schema: args.schema,
        existenceGuard: ifNotExistsGuard(args.ifNotExists),
      }),
    );
  }
}

function recordDropColumn(table, column, args) {
  push(
    compact({
      op: "dropColumn",
      table,
      column,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordRenameColumn(table, from, to, type, args) {
  push(
    compact({
      op: "renameColumn",
      table,
      from,
      to,
      type: colTypeOf(type),
      schema: args.schema,
    }),
  );
}

function recordAlterColumn(table, name, change) {
  if (change.type !== undefined) {
    push(
      compact({
        op: "alterColumnType",
        table,
        column: name,
        type: colTypeOf(change.type),
        using: resolveExpr(change.using),
        schema: change.schema,
      }),
    );
    return;
  }
  if (change.nullable !== undefined) {
    push(
      compact({
        op: "alterColumnNullability",
        table,
        column: name,
        nullable: change.nullable,
        schema: change.schema,
      }),
    );
    return;
  }
  throw structuredError("OP_INVALID", ".column(name).alter({…}) must carry `type` or `nullable`");
}

/** Build an `IrConstraint` of kind `fk`. **C1**: `onDelete`/`onUpdate` ARE
 *  emitted (compacted — omitted when absent, so an action-free FK is byte-
 *  identical to the pre-C1 wire image). */
function fkConstraintFromSpec(spec) {
  if (!spec || typeof spec !== "object" || !spec.references) {
    throw structuredError("OP_INVALID", ".foreignKey(name).add needs { columns, references:{ table, columns } }");
  }
  return compact({
    name: spec.name,
    kind: compact({
      kind: "fk",
      columns: spec.columns,
      referencesTable: spec.references.table,
      referencesColumns: spec.references.columns,
      onDelete: spec.onDelete,
      onUpdate: spec.onUpdate,
    }),
  });
}

function recordAddForeignKey(table, name, args) {
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: fkConstraintFromSpec({
        name,
        columns: args.columns,
        references: args.references,
        onDelete: args.onDelete,
        onUpdate: args.onUpdate,
      }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordAddUnique(table, name, args) {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".unique(name).add needs { columns: string[] }");
  }
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name, kind: { kind: "unique", columns: args.columns } }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordAddCheck(table, name, args) {
  if (!args || args.expr === undefined) {
    throw structuredError("OP_INVALID", ".check(name).add needs { expr: (c) => Expr }");
  }
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name, kind: { kind: "check", expr: resolveExpr(args.expr) } }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropConstraint(table, name, args) {
  push(
    compact({
      op: "dropConstraint",
      table,
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateIndex(table, name, args) {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".index(name).add needs { columns: string[] }");
  }
  push(
    compact({
      op: "createIndex",
      table,
      columns: args.columns,
      name,
      unique: args.unique,
      using: args.using,
      where: resolveExpr(args.where),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropIndex(table, name, args) {
  push(
    compact({
      op: "dropIndex",
      name,
      table,
      // `unique` drives the destructive/approval gating at apply — preserved here.
      unique: args.unique,
      concurrently: args.concurrently,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordInsert(table, args) {
  let rows = args.rows;
  if (rows === undefined) {
    throw structuredError("OP_INVALID", "insert({ rows }): rows is required");
  }
  if (!Array.isArray(rows)) rows = [rows];
  const columns = rows.length > 0 ? Object.keys(rows[0]) : [];
  const positional = rows.map((r) =>
    columns.map((col) => (Object.prototype.hasOwnProperty.call(r, col) ? toIrScalar(r[col]) : null)),
  );
  push(
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

/** Normalize an `onConflict.doUpdate` `column → scalar` map through the IrScalar
 *  carrier so a bigint/Uint8Array assignment matches the Rust `IrOnConflict`
 *  `BTreeMap<String, IrScalar>` shape. */
function normalizeOnConflict(oc) {
  if (oc === undefined || oc === null) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns };
  const doUpdate = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrScalar(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate };
}

function recordUpdate(table, args) {
  push(
    compact({
      op: "update",
      table,
      set: resolveSet(args.set),
      where: resolveExpr(args.where),
      batch: args.batch,
      schema: args.schema,
    }),
  );
}

function recordDel(table, args) {
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "del({ where }): where is mandatory (no unfiltered delete)");
  }
  push(compact({ op: "delete", table, where: resolveExpr(args.where), limit: args.limit, schema: args.schema }));
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

function recordBackfill(table, args) {
  if (args.set === undefined) {
    throw structuredError("OP_INVALID", "backfill({ set }): set is required");
  }
  push(
    compact({
      op: "backfill",
      table,
      cursorColumn: args.cursorColumn || DEFAULT_BACKFILL_CURSOR,
      batchSize: args.batchSize !== undefined ? args.batchSize : DEFAULT_BACKFILL_BATCH,
      set: resolveSet(args.set),
      filter: resolveExpr(args.where),
      name: args.name || `backfill_${table}`,
      schema: args.schema,
    }),
  );
}

// ===========================================================================
// (E) The fluent `table()` handle — the SOLE public entry (§3). The byte-for-byte
// twin of `sdks/migrate/src/ops.ts`'s `table()`. A reusable value carrying only
// `{ name, schemaDefault }`; terminals record EAGERLY and return the handle, so it
// is valid for unlimited chaining + var-reuse (§4). A per-op `schema` overrides
// the table default by key presence.
// ===========================================================================

/** Per-op-wins-over-table-default schema precedence (§3/§4). */
function pickSchema(perCall, dflt) {
  if (perCall && perCall.schema !== undefined) return perCall.schema;
  return dflt;
}

function requireColumnDef(x, where) {
  if (!isColumnDef(x)) {
    throw structuredError("OP_INVALID", `${where} must be a t.* ColumnDef`);
  }
}

export function table(name, opts = {}) {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;

  const handle = {
    // §3.1 — the table itself
    create(args) {
      recordCreateTable(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    drop(args = {}) {
      recordDropTable(name, {
        ifExists: args.ifExists,
        cascade: args.cascade,
        schema: pickSchema(args, dflt),
      });
      return handle;
    },

    // §3.2 — columns
    column(col) {
      requireString(col, ".column(name)");
      const id = registerSelector("column", col);
      return {
        add(args) {
          requireColumnDef(args.type, ".column(name).add({ type })");
          terminateSelector(id);
          recordAddColumn(name, col, args.type, {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropColumn(name, col, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        },
        rename(args) {
          requireString(args.to, ".column(name).rename({ to })");
          requireColumnDef(args.type, ".column(name).rename({ type })");
          terminateSelector(id);
          recordRenameColumn(name, col, args.to, args.type, { schema: pickSchema(args, dflt) });
          return handle;
        },
        alter(args) {
          terminateSelector(id);
          recordAlterColumn(name, col, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },

    // §3.3 — constraints
    foreignKey(fkName) {
      requireString(fkName, ".foreignKey(name)");
      const id = registerSelector("foreignKey", fkName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddForeignKey(name, fkName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    unique(uqName) {
      requireString(uqName, ".unique(name)");
      const id = registerSelector("unique", uqName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddUnique(name, uqName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    check(ckName) {
      requireString(ckName, ".check(name)");
      const id = registerSelector("check", ckName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddCheck(name, ckName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    constraint(cName) {
      requireString(cName, ".constraint(name)");
      const id = registerSelector("constraint", cName);
      return {
        drop(args = {}) {
          terminateSelector(id);
          recordDropConstraint(name, cName, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },

    // §3.4 — indexes
    index(idxName) {
      requireString(idxName, ".index(name)");
      const id = registerSelector("index", idxName);
      return {
        add(args) {
          terminateSelector(id);
          recordCreateIndex(name, idxName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropIndex(name, idxName, {
            ifExists: args.ifExists,
            concurrently: args.concurrently,
            unique: args.unique,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
      };
    },

    // §3.5 — table data (no existence guard; schema rides on args)
    insert(args) {
      recordInsert(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    update(args) {
      recordUpdate(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    del(args) {
      recordDel(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    backfill(args) {
      recordBackfill(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
  };

  return handle;
}

// ===========================================================================
// (C) Determinism lint. Flag the JS nondeterminism accessors (`Date.now()` /
// `Math.random()` / `crypto.randomUUID()` / `new Date()`), steering authors to
// the DB-evaluated `c.fn.now()` / `c.fn.genRandomUuid()` (`FnSynth`) scalars. A
// best-effort, AST-free SOURCE scan; the authoritative determinism guarantee is
// the build-once committed artifact, this is the pre-commit catch.
// ===========================================================================

const NONDETERMINISM_PATTERNS = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "c.fn.genRandomUuid() (for an id) or a DB-evaluated value" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "c.fn.now()" },
];

/**
 * Lint a migration's SOURCE TEXT for the nondeterminism accessors. Returns an
 * array of `{ code, accessor, suggested_fix, reason }` findings (empty ⇒ clean).
 *
 * SCOPE — intentional coarse whole-source scan: it OVER-flags (a clock accessor
 * in a comment / a non-op helper trips it) and NEVER under-flags. The record path
 * surfaces these as WARNINGS, never a hard reject.
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
          "must be computed at apply time use the structured FnSynth scalar",
      });
    }
  }
  return findings;
}

// ---------------------------------------------------------------------------
// `c.fn.splitPart` grammar lint (§9) — the dialect-NEUTRAL subset broken on BOTH
// backends (a non-string/empty delimiter, a non-positive-int n). A violation
// throws a structured EXPR_NOT_PORTABLE error. The portability ENVELOPE
// (single-ASCII delimiter, 1<=n<=8) is dialect-gated and deferred to the Rust
// validator.
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
