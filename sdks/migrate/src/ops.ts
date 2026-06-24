// `@zeroship/migrate` — the op-builder DSL implementation (§3.2/§3.3.1, PR3).
//
// This is the TS authoring surface a creator imports:
//   import { createTable, addColumn, backfill, t } from "@zeroship/migrate";
//
// It is the typed peer of the engine-embedded recorder
// (`crates/zeroship-migrate-js/src/migrate_ops.js`, which the Rust runtime
// `include_str!`s into V8 at build/record time). Both emit the IDENTICAL
// dialect-neutral op objects the closed Rust `Op` enum / `op-ir.schema.json`
// deserialize — the `.ir.json` wire shape is frozen, and the golden corpus
// (`tests/op_fixtures`) + the `Checksum::of_ir` round-trip are the contract.
//
// Each op-function RECORDS one canonical op object onto the ambient per-migration
// recorder (§3.1), synchronously, returning void. Calling an op-function OUTSIDE
// an active recorder throws a structured `OP_OUTSIDE_RECORDER`.

import type {
  AlterColumnChange,
  BackfillArgs,
  BatchAlterBuilder,
  CheckSpec,
  ColType,
  ColumnDef as ColumnDefType,
  ColumnOpts,
  CreateIndexSpec,
  DelArgs,
  DeterminismFinding,
  DropConstraintSpec,
  ExprBuilder,
  ExprChain as ExprChainType,
  ExprFn,
  FnNamespace,
  ForeignKeySpec,
  InsertArgs,
  Row,
  ScalarValue,
  TableBuilder,
  TypeLexicon,
  UniqueSpec,
  UpdateArgs,
} from "./types.js";

import { TypeBuilder as DbTypeBuilder } from "@zeroship/db";

import { colTypeFromDbField, type DbSchemaField } from "./db-lexicon.js";

type Node = Record<string, unknown>;

// ── The ambient recorder (§3.1) ──

let active: Node[] | null = null;

function structuredError(code: string, message: string, extra?: Record<string, unknown>): Error {
  const err = new Error(message) as Error & Record<string, unknown>;
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

/** Begin a fresh recording buffer (the build evaluator calls this before a phase). */
export function __begin(): void {
  active = [];
}

/** Drain + return the recorded op list, clearing the active recorder. */
export function __drain(): Node[] {
  if (active === null) return [];
  const out = active;
  active = null;
  return out;
}

function push(op: Node): Node {
  if (active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      `op-function "${String(op.op)}" called outside an active migration recorder; ` +
        "op-functions may only be called synchronously inside up()/down()",
      { suggested_fix: "move the op-function call inside the migration's up()/down() body" },
    );
  }
  active.push(op);
  return op;
}

function compact<T extends Record<string, unknown>>(obj: T): T {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function requireString(v: unknown, what: string): asserts v is string {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

// ── (B) The chainable `t.*` lexicon ──

class ColumnDefImpl implements ColumnDefType {
  _type: ColType;
  _nullable = true;
  _default: unknown = undefined;
  _primaryKey = false;
  _unique = false;

  constructor(colType: ColType) {
    this._type = colType;
  }
  notNull(): this {
    this._nullable = false;
    return this;
  }
  default(value: ScalarValue | { fn: "now" | "genRandomUuid" }): this {
    this._default = toIrDefault(value);
    return this;
  }
  ref(targetTable: string): this {
    requireString(targetTable, "t.*.ref(target)");
    this._type = { ref: { references: targetTable } } as ColType;
    return this;
  }
  primaryKey(): this {
    this._primaryKey = true;
    this._nullable = false;
    return this;
  }
  unique(): this {
    this._unique = true;
    return this;
  }
  __toIrColumn(name: string): Node {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      unique: this._unique ? true : undefined,
    });
  }
  __toAddColumnTail(): Node {
    return compact({
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
    });
  }
}

function isColumnDef(x: unknown): x is ColumnDefImpl {
  return x instanceof ColumnDefImpl;
}

function applyOpts(def: ColumnDefImpl, opts?: ColumnOpts): ColumnDefImpl {
  if (opts === undefined || opts === null) return def;
  if (opts.notNull) def.notNull();
  if (opts.primaryKey) def.primaryKey();
  if (opts.unique) def.unique();
  if (opts.ref !== undefined) def.ref(opts.ref);
  if (opts.default !== undefined) def.default(opts.default);
  return def;
}

/** Base64-encode raw bytes (the `IrScalar::Bytes` wire carrier) without a Node
 *  `Buffer` dependency — runs identically in the V8 record host and Node. */
function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
  // `btoa` is a WHATWG global present in V8 + Node ≥16.
  return btoa(bin);
}

/**
 * Normalize a JS scalar into the closed `IrScalar` WIRE carrier so the recorded
 * shape is exactly what Rust's `IrScalar` deserializer accepts (§2.5/§3.2):
 *  - a JS `bigint` → `{ decimal: "<v>" }` (a bare bigint THROWS at JSON.stringify;
 *    `{decimal}` is the integers-beyond-2^53 carrier);
 *  - a `Uint8Array` → `{ bytes: "<base64>" }` (the raw-bytes carrier; the default
 *    JSON spelling `{"0":…}` is HARD-REJECTED by the Rust deserializer);
 *  - everything else (string / safe number / boolean / null / the explicit
 *    `{decimal}` / `{bytes}` carriers) passes through verbatim.
 */
function toIrScalar(value: unknown): unknown {
  if (typeof value === "bigint") return { decimal: value.toString() };
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  return value;
}

function toIrDefault(value: ScalarValue | { fn: "now" | "genRandomUuid" }): Node {
  if (value && typeof value === "object" && "fn" in value && typeof value.fn === "string") {
    return { fn: { fn: value.fn } };
  }
  return { literal: { value: toIrScalar(value) } };
}

export const t: TypeLexicon = {
  id: () => {
    const d = new ColumnDefImpl("uuid");
    d.primaryKey();
    d.default({ fn: "genRandomUuid" });
    return d;
  },
  text: (opts) => applyOpts(new ColumnDefImpl("text"), opts),
  numeric: (precision = 38, scale = 9, opts) =>
    applyOpts(new ColumnDefImpl({ decimal: { precision, scale } } as ColType), opts),
  timestamp: (opts) => applyOpts(new ColumnDefImpl("timestamp"), opts),
  uuid: (opts) => applyOpts(new ColumnDefImpl("uuid"), opts),
  bytes: (opts) => applyOpts(new ColumnDefImpl("bytea"), opts),
  boolean: (opts) => applyOpts(new ColumnDefImpl("bool"), opts),
  json: (opts) => applyOpts(new ColumnDefImpl("json"), opts),
  ref: (targetTable, opts) => {
    requireString(targetTable, "t.ref(target)");
    return applyOpts(new ColumnDefImpl({ ref: { references: targetTable } } as ColType), opts);
  },
  vector: (n, opts) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    return applyOpts(new ColumnDefImpl({ vector: { vector: n } } as ColType), opts);
  },
  geoPoint: (opts) => applyOpts(new ColumnDefImpl("geoPoint"), opts),
  string: (opts) => applyOpts(new ColumnDefImpl("string"), opts),
  int: (opts) => applyOpts(new ColumnDefImpl("int"), opts),
  integer: (opts) => applyOpts(new ColumnDefImpl("int"), opts),
  bigInt: (opts) => applyOpts(new ColumnDefImpl("bigInt"), opts),
  float: (opts) => applyOpts(new ColumnDefImpl("float"), opts),
  encrypted: (arg, opts) => {
    const inner = arg && typeof arg === "object" && "of" in arg ? (arg as { of: unknown }).of : arg;
    const innerType = isColumnDef(inner) ? inner._type : (inner as ColType);
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return applyOpts(new ColumnDefImpl({ encrypted: { of: innerType } } as ColType), opts);
  },
};

function colTypeOf(typeArg: ColumnDefType | ColType): ColType {
  if (isColumnDef(typeArg)) return typeArg._type;
  return typeArg as ColType;
}

// ── (A) The shared `@zeroship/db` lexicon bridge (PR5) ──

/**
 * Lift a `@zeroship/db` schema field (a `t.*` `TypeBuilder` or its `FieldDef`)
 * into a migration `ColumnDef`, so a column declared in the live `@zeroship/db`
 * schema lowers through the IDENTICAL `ColType` path a hand-written migration
 * column does (PR5 goal A — one shared lexicon). The TYPE is bridged via the
 * single-source {@link colTypeFromDbField} reduction; the column's NULLABILITY is
 * carried over (`@zeroship/db` `.required()` → migration `.notNull()`; otherwise
 * nullable, matching the migration DSL default). Table/column NAMES are NEVER
 * bound to the live schema — a `t.ref(target)` keeps its target as a plain string
 * (§3.3). Returns a chainable `ColumnDef`, so a caller can still layer migration
 * modifiers (`.default(...)`, `.primaryKey()`, …) on top.
 */
export function fromDb(field: DbSchemaField): ColumnDefType {
  const def = new ColumnDefImpl(colTypeFromDbField(field));
  const fd = field instanceof DbTypeBuilder ? field.toFieldDef() : field;
  if (fd && typeof fd === "object" && (fd as { required?: boolean }).required === true) {
    def.notNull();
  }
  if (fd && typeof fd === "object" && (fd as { unique?: boolean }).unique === true) {
    def.unique();
  }
  return def;
}

// ── (B) The fluent `(c) => Expr` builder ──

function chain(node: Node): ExprChainImpl {
  return new ExprChainImpl(node);
}

function exprArg(x: unknown): Node {
  if (x instanceof ExprChainImpl) return x.__node;
  if (x && typeof x === "object" && typeof (x as Node).node === "string") return x as Node;
  return { node: "literal", value: x as unknown };
}

class ExprChainImpl implements ExprChainType {
  __node: Node;
  constructor(node: Node) {
    this.__node = node;
  }
  private bin(op: string, x: unknown): ExprChainImpl {
    return chain({ node: "binOp", op, lhs: this.__node, rhs: exprArg(x) });
  }
  eq(x: unknown) { return this.bin("eq", x); }
  ne(x: unknown) { return this.bin("ne", x); }
  lt(x: unknown) { return this.bin("lt", x); }
  le(x: unknown) { return this.bin("le", x); }
  gt(x: unknown) { return this.bin("gt", x); }
  ge(x: unknown) { return this.bin("ge", x); }
  and(e: unknown) { return this.bin("and", e); }
  or(e: unknown) { return this.bin("or", e); }
  not() { return chain({ node: "unaryOp", op: "not", operand: this.__node }); }
  add(x: unknown) { return this.bin("add", x); }
  sub(x: unknown) { return this.bin("sub", x); }
  mul(x: unknown) { return this.bin("mul", x); }
  div(x: unknown) { return this.bin("div", x); }
  concat(...parts: unknown[]) {
    let acc = this.__node;
    for (const p of parts) acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    return chain(acc);
  }
  concatWs(sep: unknown, ...parts: unknown[]) {
    return chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), this.__node, ...parts.map(exprArg)] });
  }
  coalesce(...args: unknown[]) {
    return chain({ node: "fnCall", fn: "coalesce", args: [this.__node, ...args.map(exprArg)] });
  }
  isNull() { return chain({ node: "unaryOp", op: "isNull", operand: this.__node }); }
  isNotNull() { return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node }); }
  isTrue() { return chain({ node: "unaryOp", op: "isTrue", operand: this.__node }); }
  isFalse() { return chain({ node: "unaryOp", op: "isFalse", operand: this.__node }); }
  cast(target: "text" | "integer" | "real" | "boolean" | "blob") {
    return chain({ node: "cast", operand: this.__node, target });
  }
}

const fn: FnNamespace = {
  lower: (e) => chain({ node: "fnCall", fn: "lower", args: [exprArg(e)] }),
  upper: (e) => chain({ node: "fnCall", fn: "upper", args: [exprArg(e)] }),
  trim: (e) => chain({ node: "fnCall", fn: "trim", args: [exprArg(e)] }),
  length: (e) => chain({ node: "fnCall", fn: "length", args: [exprArg(e)] }),
  abs: (e) => chain({ node: "fnCall", fn: "abs", args: [exprArg(e)] }),
  coalesce: (...args) => chain({ node: "fnCall", fn: "coalesce", args: args.map(exprArg) }),
  nullif: (a, b) => chain({ node: "fnCall", fn: "nullif", args: [exprArg(a), exprArg(b)] }),
  concatWs: (sep, ...parts) => chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),
  case: (branches, elseVal) => {
    if (!Array.isArray(branches)) {
      throw structuredError("OP_INVALID", "c.fn.case(branches, else?): branches must be an array of [cond, result]");
    }
    const node: Node = {
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
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },
  now: () => chain({ node: "fnSynth", fn: "now", args: [] }),
  genRandomUuid: () => chain({ node: "fnSynth", fn: "genRandomUuid", args: [] }),
};

function makeBuilder(): ExprBuilder {
  const c = ((name: string) => {
    requireString(name, 'c("name")');
    return chain({ node: "colRef", name });
  }) as unknown as ExprBuilder;
  c.fn = fn;
  return c;
}

function resolveExpr(slot: ExprFn | ExprChainType | Node | undefined): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  if (typeof slot === "function") return exprArg(slot(makeBuilder()));
  if (slot instanceof ExprChainImpl) return slot.__node;
  if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") return slot as Node;
  throw structuredError("OP_INVALID", "expression slot must be a (c) => Expr callback or a built expression");
}

function resolveSet(set: Record<string, ExprFn>): Record<string, Node> {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column → expression");
  }
  const out: Record<string, Node> = {};
  for (const col of Object.keys(set)) out[col] = resolveExpr(set[col])!;
  return out;
}

// ── (A) The op-functions ──

export function createTable(
  name: string,
  columns: Record<string, ColumnDefType>,
  build?: (b: TableBuilder) => void,
): void {
  requireString(name, "createTable(name, …)");
  const cols: Node[] = [];
  const constraints: Node[] = [];
  const indexes: Node[] = [];
  const pkCols: string[] = [];

  for (const colName of Object.keys(columns)) {
    const def = columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError("OP_INVALID", `createTable column "${colName}" must be a t.* ColumnDef`);
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  if (pkCols.length > 0) constraints.push({ kind: { kind: "pk", columns: pkCols } });

  if (typeof build === "function") build(makeTableBuilder(constraints, indexes));

  push(
    compact({
      op: "createTable",
      name,
      columns: cols,
      constraints: constraints.length ? constraints : undefined,
      indexes: indexes.length ? indexes : undefined,
    }),
  );
}

function makeTableBuilder(constraints: Node[], indexes: Node[]): TableBuilder {
  return {
    index(columns, opts = {}) {
      indexes.push(
        compact({ name: opts.name, columns, unique: opts.unique, using: opts.using, where: resolveExpr(opts.where) }),
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

export function dropTable(table: string, opts: { ifExists?: boolean; cascade?: boolean } = {}): void {
  requireString(table, "dropTable(table)");
  push(compact({ op: "dropTable", table, ifExists: opts.ifExists, cascade: opts.cascade }));
}

export function addColumn(table: string, name: string, type: ColumnDefType): void {
  requireString(table, "addColumn(table, …)");
  requireString(name, "addColumn(table, name, …)");
  if (!isColumnDef(type)) {
    throw structuredError("OP_INVALID", "addColumn type must be a t.* ColumnDef");
  }
  push(compact({ op: "addColumn", table, column: name, ...type.__toAddColumnTail() }));
}

export function dropColumn(table: string, column: string, opts: { ifExists?: boolean } = {}): void {
  requireString(table, "dropColumn(table, …)");
  requireString(column, "dropColumn(table, column)");
  push(compact({ op: "dropColumn", table, column, ifExists: opts.ifExists }));
}

export function renameColumn(table: string, from: string, to: string, type: ColumnDefType): void {
  requireString(table, "renameColumn(table, …)");
  requireString(from, "renameColumn from");
  requireString(to, "renameColumn to");
  push({ op: "renameColumn", table, from, to, type: colTypeOf(type) });
}

export function alterColumn(table: string, name: string, change: AlterColumnChange): void {
  requireString(table, "alterColumn(table, …)");
  requireString(name, "alterColumn(table, name, …)");
  if (!change || typeof change !== "object") {
    throw structuredError("OP_INVALID", "alterColumn change must be an object");
  }
  if (change.type !== undefined) {
    push(
      compact({
        op: "alterColumnType",
        table,
        column: name,
        type: colTypeOf(change.type),
        using: resolveExpr(change.using),
      }),
    );
    return;
  }
  if (change.nullable !== undefined) {
    push({ op: "alterColumnNullability", table, column: name, nullable: change.nullable });
    return;
  }
  throw structuredError("OP_INVALID", "alterColumn change must carry `type` or `nullable`");
}

function fkConstraintFromSpec(spec: ForeignKeySpec): Node {
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

export function addForeignKey(table: string, spec: ForeignKeySpec): void {
  requireString(table, "addForeignKey(table, …)");
  push({ op: "addConstraint", table, constraint: fkConstraintFromSpec(spec) });
}

export function addUnique(table: string, spec: UniqueSpec): void {
  requireString(table, "addUnique(table, …)");
  if (!spec || !Array.isArray(spec.columns)) {
    throw structuredError("OP_INVALID", "addUnique spec needs { columns: string[], name? }");
  }
  push({ op: "addConstraint", table, constraint: compact({ name: spec.name, kind: { kind: "unique", columns: spec.columns } }) });
}

export function addCheck(table: string, spec: CheckSpec): void {
  requireString(table, "addCheck(table, …)");
  if (!spec || spec.expr === undefined) {
    throw structuredError("OP_INVALID", "addCheck spec needs { expr: (c) => Expr, name? }");
  }
  push({ op: "addConstraint", table, constraint: compact({ name: spec.name, kind: { kind: "check", expr: resolveExpr(spec.expr) } }) });
}

export function dropConstraint(table: string, spec: DropConstraintSpec | string): void {
  requireString(table, "dropConstraint(table, …)");
  const name = typeof spec === "string" ? spec : spec && spec.name;
  requireString(name, "dropConstraint name");
  push({ op: "dropConstraint", table, name });
}

export function createIndex(table: string, spec: CreateIndexSpec): void {
  requireString(table, "createIndex(table, …)");
  if (!spec || !Array.isArray(spec.columns)) {
    throw structuredError("OP_INVALID", "createIndex spec needs { columns: string[], … }");
  }
  push(
    compact({
      op: "createIndex",
      table,
      columns: spec.columns,
      name: spec.name,
      unique: spec.unique,
      using: spec.using,
      where: resolveExpr(spec.where),
      concurrently: spec.concurrently,
    }),
  );
}

export function dropIndex(
  name: string,
  opts: { table?: string; unique?: boolean; ifExists?: boolean; concurrently?: boolean } = {},
): void {
  requireString(name, "dropIndex(name, …)");
  push(
    compact({
      op: "dropIndex",
      name,
      table: opts.table,
      unique: opts.unique,
      ifExists: opts.ifExists,
      concurrently: opts.concurrently,
    }),
  );
}

export function insert<R extends Row = Row>(table: string, args: InsertArgs<R>): void {
  requireString(table, "insert(table, …)");
  let rows = args.rows as R | R[];
  if (rows === undefined) throw structuredError("OP_INVALID", "insert(table, { rows }): rows is required");
  if (!Array.isArray(rows)) rows = [rows];
  const arr = rows as R[];
  const columns = arr.length > 0 ? Object.keys(arr[0]) : [];
  const positional = arr.map((r) =>
    columns.map((col) =>
      Object.prototype.hasOwnProperty.call(r, col) ? toIrScalar((r as Row)[col]) : null,
    ),
  );
  push(compact({ op: "insert", table, columns, rows: positional, onConflict: normalizeOnConflict(args.onConflict) }));
}

/** Normalize an `onConflict.doUpdate` `column → scalar` map through the IrScalar
 *  carrier so a bigint/Uint8Array assignment matches the Rust `IrOnConflict`
 *  `BTreeMap<String, IrScalar>` shape (§2.4.1). */
function normalizeOnConflict(
  oc: { columns: string[]; doUpdate?: Record<string, unknown> } | undefined,
): Node | undefined {
  if (oc === undefined) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns } as Node;
  const doUpdate: Record<string, unknown> = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrScalar(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate } as Node;
}

export function update(table: string, args: UpdateArgs): void {
  requireString(table, "update(table, …)");
  push(
    compact({
      op: "update",
      table,
      set: resolveSet(args.set),
      where: resolveExpr(args.where),
      batch: args.batch,
    }),
  );
}

export function del(table: string, args: DelArgs): void {
  requireString(table, "del(table, …)");
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "del(table, { where }): where is mandatory");
  }
  push(compact({ op: "delete", table, where: resolveExpr(args.where), limit: args.limit }));
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

export function backfill(table: string, args: BackfillArgs): void {
  requireString(table, "backfill(table, …)");
  if (args.set === undefined) throw structuredError("OP_INVALID", "backfill(table, { set }): set is required");
  push(
    compact({
      op: "backfill",
      table,
      cursorColumn: args.cursorColumn || DEFAULT_BACKFILL_CURSOR,
      batchSize: args.batchSize !== undefined ? args.batchSize : DEFAULT_BACKFILL_BATCH,
      set: resolveSet(args.set),
      filter: resolveExpr(args.where),
      name: args.name || `backfill_${table}`,
    }),
  );
}

export function batchAlterTable(table: string, build: (b: BatchAlterBuilder) => void): void {
  requireString(table, "batchAlterTable(table, …)");
  if (typeof build !== "function") {
    throw structuredError("OP_INVALID", "batchAlterTable(table, build): build must be a callback");
  }
  build({
    addColumn: (name, type) => addColumn(table, name, type),
    dropColumn: (name, opts) => dropColumn(table, name, opts),
    renameColumn: (from, to, type) => renameColumn(table, from, to, type),
    alterColumn: (name, change) => alterColumn(table, name, change),
    addForeignKey: (spec) => addForeignKey(table, spec),
    addCheck: (spec) => addCheck(table, spec),
  });
}

// ── (C) Determinism lint (§4.3) ──

const NONDETERMINISM_PATTERNS: { re: RegExp; name: string; steer: string }[] = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "c.fn.genRandomUuid() or a DB-evaluated value" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "c.fn.now()" },
];

/**
 * Lint a migration's SOURCE for the §4.3 nondeterminism accessors (`Date.now()` /
 * `Math.random()` / `crypto.randomUUID()` / `new Date()`).
 *
 * SCOPE — intentional coarse whole-source scan. §4.3 specifies "syntactically
 * appearing inside an op-function argument", but this lint is a deliberate
 * fail-SAFE whole-source regex scan: it OVER-flags (a clock accessor in a comment
 * or a non-op helper trips it) and NEVER under-flags. That is the chosen contract,
 * not a bug — the build-once committed artifact (§5.1) already neutralizes
 * post-deploy non-determinism, so the lint's only job is a best-effort pre-commit
 * STEER (§8.8), where a false positive is cheap (rephrase) and a false negative
 * (a baked build-time value slipping through) is the real hazard. Findings are
 * surfaced as WARNINGS on the record path, never a hard reject (see
 * `record_migration_to_ir_with_warnings`). Narrowing to true op-arg spans is a
 * possible future precision improvement; the over-flag behavior is pinned by test.
 */
export function lintDeterminism(source: string): DeterminismFinding[] {
  if (typeof source !== "string") return [];
  const findings: DeterminismFinding[] = [];
  for (const { re, name, steer } of NONDETERMINISM_PATTERNS) {
    if (re.test(source)) {
      findings.push({
        code: "NONDETERMINISTIC_OP_ARG",
        accessor: name,
        suggested_fix: `replace ${name} with the DB-evaluated ${steer}`,
        reason: `${name} bakes a build-time value into the artifact; use the structured FnSynth scalar (§4.3)`,
      });
    }
  }
  return findings;
}

function splitPartGrammarLint(delim: unknown, n: unknown): void {
  const fail = (reason: string) => {
    throw structuredError("EXPR_NOT_PORTABLE", reason, {
      suggested_fix:
        "pass a non-empty string-literal delimiter and a positive-integer n; to target SQLite, " +
        "stay in-envelope (single-ASCII delimiter, 1<=n<=8)",
    });
  };
  if (typeof delim !== "string") fail(`c.fn.splitPart delimiter must be a string literal; got ${typeof delim}`);
  if ((delim as string).length === 0) fail("c.fn.splitPart delimiter must be a non-empty string literal");
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`c.fn.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if ((n as number) < 1) fail(`c.fn.splitPart part index n must be a positive integer; got ${n}`);
}
