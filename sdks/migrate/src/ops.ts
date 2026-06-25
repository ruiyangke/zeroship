// `@zeroship/migrate` — the fluent-only op-builder DSL implementation
// (design `2026-06-25-op-dsl-fluent-redesign.md`).
//
// This is the TS authoring surface a creator imports:
//   import { table, t } from "@zeroship/migrate";
//
//   export default {
//     up() {
//       table("users")
//         .column("first_name").add({ type: t.text() })
//         .backfill({ set: { first_name: c => c.fn.splitPart(c("name"), " ", 1) } });
//     },
//   };
//
// It is the typed peer of the engine-embedded recorder
// (`crates/zeroship-migrate-js/src/migrate_ops.js`, which the Rust runtime
// `include_str!`s into V8 at build/record time). Both emit the IDENTICAL
// dialect-neutral op objects the closed Rust `Op` enum / `op-ir.schema.json`
// deserialize — the `.ir.json` wire shape is frozen (byte-identical to the
// pre-redesign flat surface except the C1 FK-actions delta), and the golden
// corpus (`tests/op_fixtures`) + the `Checksum::of_ir` round-trip are the
// contract.
//
// `table()` is the SOLE public entry. The flat op-functions are GONE from the
// public API; their op-construction logic survives as the internal `recordX`
// helpers the handle delegates to (so the IR is unchanged). Every terminal
// RECORDS one canonical op object onto the ambient per-migration recorder
// synchronously, returning the handle. Recording OUTSIDE an active recorder
// throws a structured `OP_OUTSIDE_RECORDER`.

import type {
  BackfillArgs,
  CheckRef,
  ColType,
  ColumnDef as ColumnDefType,
  ColumnRef,
  ConstraintRef,
  CreateTableArgs,
  DelArgs,
  DeterminismFinding,
  ExprBuilder,
  ExprChain as ExprChainType,
  ExprFn,
  FnNamespace,
  ForeignKeyRef,
  ForeignKeyReference,
  IndexRef,
  InsertArgs,
  RefAction,
  Row,
  ScalarValue,
  TableHandle,
  TableOptions,
  TypeLexicon,
  UniqueRef,
  UpdateArgs,
} from "./types.js";

import { TypeBuilder as DbTypeBuilder } from "@zeroship/db";

import { colTypeFromDbField, type DbSchemaField } from "./db-lexicon.js";

type Node = Record<string, unknown>;

// ── The ambient recorder (§3.1 / §5) ──

/** A handed-out, not-yet-terminated selector the recorder tracks (§5). */
interface PendingSelector {
  selector: string;
  name: string;
  terminated: boolean;
}

interface Recorder {
  ops: Node[];
  /** Every selector the handle handed out this phase, keyed by a monotonic id. */
  pending: Map<number, PendingSelector>;
  nextSelectorId: number;
}

let active: Recorder | null = null;

function structuredError(code: string, message: string, extra?: Record<string, unknown>): Error {
  const err = new Error(message) as Error & Record<string, unknown>;
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

/** Begin a fresh recording buffer (the build evaluator calls this before a phase). */
export function __begin(): void {
  active = { ops: [], pending: new Map(), nextSelectorId: 0 };
}

/**
 * Drain + return the recorded op list, clearing the active recorder. At DRAIN
 * (not eagerly — so a var-held selector terminated on a later line is fine, §5),
 * any selector that was handed out but never terminated is a hard structured
 * `SELECTOR_NOT_TERMINATED` error.
 */
export function __drain(): Node[] {
  if (active === null) return [];
  const rec = active;
  active = null;
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

function recorder(): Recorder {
  if (active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      "op authoring called outside an active migration recorder; " +
        "the table() handle may only be used synchronously inside up()/down()",
      { suggested_fix: "move the table()/selector calls inside the migration's up()/down() body" },
    );
  }
  return active;
}

function push(op: Node): Node {
  recorder().ops.push(op);
  return op;
}

/** Register a handed-out selector; returns its id (used at terminate). */
function registerSelector(selector: string, name: string): number {
  const rec = recorder();
  const id = rec.nextSelectorId++;
  rec.pending.set(id, { selector, name, terminated: false });
  return id;
}

/** Mark a selector terminated; double-terminate is a structured error (§5). */
function terminateSelector(id: number): void {
  const rec = recorder();
  const sel = rec.pending.get(id);
  // `sel` is always present for a live id; defensive only.
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

// ── (B) The IMMUTABLE chainable `t.*` lexicon (§4) ──

class ColumnDefImpl implements ColumnDefType {
  readonly _type: ColType;
  readonly _nullable: boolean;
  readonly _default: unknown;
  readonly _primaryKey: boolean;
  readonly _unique: boolean;

  constructor(
    colType: ColType,
    fields?: { nullable?: boolean; default?: unknown; primaryKey?: boolean; unique?: boolean },
  ) {
    this._type = colType;
    this._nullable = fields?.nullable ?? true;
    this._default = fields?.default;
    this._primaryKey = fields?.primaryKey ?? false;
    this._unique = fields?.unique ?? false;
  }

  /** Clone with the named fields overridden — the basis of immutability (§4). */
  private with(over: {
    type?: ColType;
    nullable?: boolean;
    default?: unknown;
    primaryKey?: boolean;
    unique?: boolean;
  }): ColumnDefImpl {
    return new ColumnDefImpl(over.type ?? this._type, {
      nullable: over.nullable ?? this._nullable,
      default: "default" in over ? over.default : this._default,
      primaryKey: over.primaryKey ?? this._primaryKey,
      unique: over.unique ?? this._unique,
    });
  }

  notNull(): ColumnDefImpl {
    return this.with({ nullable: false });
  }
  default(value: ScalarValue | { fn: "now" | "genRandomUuid" }): ColumnDefImpl {
    return this.with({ default: toIrDefault(value) });
  }
  ref(targetTable: string): ColumnDefImpl {
    requireString(targetTable, "t.*.ref(target)");
    return this.with({ type: { ref: { references: targetTable } } as ColType });
  }
  primaryKey(): ColumnDefImpl {
    return this.with({ primaryKey: true, nullable: false });
  }
  unique(): ColumnDefImpl {
    return this.with({ unique: true });
  }

  __toIrColumn(name: string): Node {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      // C2 — a PRIMARY KEY already IMPLIES uniqueness, so a column that is BOTH
      // `.unique()` and `.primaryKey()` would otherwise carry a redundant
      // column-level UNIQUE (an extra index/constraint) on top of the table's pk
      // constraint. Suppress it (lock-step with the addColumn path + the differ,
      // which never emits a separate UNIQUE for the PK column).
      unique: this._unique && !this._primaryKey ? true : undefined,
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
 * shape is exactly what Rust's `IrScalar` deserializer accepts (§3.5):
 *  - a JS `bigint` → `{ decimal: "<v>" }` (a bare bigint THROWS at JSON.stringify);
 *  - a `Uint8Array` → `{ bytes: "<base64>" }` (the raw-bytes carrier);
 *  - everything else passes through verbatim.
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
  id: () => new ColumnDefImpl("uuid").primaryKey().default({ fn: "genRandomUuid" }),
  text: () => new ColumnDefImpl("text"),
  numeric: (precision = 38, scale = 9) =>
    new ColumnDefImpl({ decimal: { precision, scale } } as ColType),
  timestamp: () => new ColumnDefImpl("timestamp"),
  uuid: () => new ColumnDefImpl("uuid"),
  bytes: () => new ColumnDefImpl("bytea"),
  boolean: () => new ColumnDefImpl("bool"),
  json: () => new ColumnDefImpl("json"),
  ref: (targetTable) => {
    requireString(targetTable, "t.ref(target)");
    return new ColumnDefImpl({ ref: { references: targetTable } } as ColType);
  },
  vector: (n) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    return new ColumnDefImpl({ vector: { vector: n } } as ColType);
  },
  geoPoint: () => new ColumnDefImpl("geoPoint"),
  integer: () => new ColumnDefImpl("int"),
  bigInt: () => new ColumnDefImpl("bigInt"),
  float: () => new ColumnDefImpl("float"),
  encrypted: (arg) => {
    const inner = arg && typeof arg === "object" && "of" in arg ? (arg as { of: unknown }).of : arg;
    const innerType = isColumnDef(inner) ? inner._type : (inner as ColType);
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return new ColumnDefImpl({ encrypted: { of: innerType } } as ColType);
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
 * carried over (`@zeroship/db` `.required()` → migration `.notNull()`). Table/
 * column NAMES are NEVER bound to the live schema. Returns a chainable
 * (immutable) `ColumnDef`, so a caller can still layer migration modifiers on top.
 */
export function fromDb(field: DbSchemaField): ColumnDefType {
  let def: ColumnDefImpl = new ColumnDefImpl(colTypeFromDbField(field));
  const fd = field instanceof DbTypeBuilder ? field.toFieldDef() : field;
  if (fd && typeof fd === "object" && (fd as { required?: boolean }).required === true) {
    def = def.notNull();
  }
  if (fd && typeof fd === "object" && (fd as { unique?: boolean }).unique === true) {
    def = def.unique();
  }
  return def;
}

// ── (B) The fluent `(c) => Expr` builder (§3.6) ──

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

// ── (C) Existence-guard token mappers ──

function ifNotExistsGuard(v: boolean | undefined): "ifNotExists" | undefined {
  return v ? "ifNotExists" : undefined;
}
function ifExistsGuard(v: boolean | undefined): "ifExists" | undefined {
  return v ? "ifExists" : undefined;
}

// ── (D) The internal op-construction helpers (the single source of truth) ──
//
// These build + push the EXACT canonical op object the Rust `Op` enum / the
// recorder twin emit (byte-identical IR except the C1 FK-actions delta). They are
// internal — only the fluent `table()` handle calls them.

function recordCreateTable(name: string, args: CreateTableArgs): void {
  const cols: Node[] = [];
  const constraints: Node[] = [];
  const indexes: Node[] = [];
  const pkCols: string[] = [];

  for (const colName of Object.keys(args.columns)) {
    const def = args.columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError("OP_INVALID", `create column "${colName}" must be a t.* ColumnDef`);
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  // A composite `primaryKey: [...]` wins over per-column `.primaryKey()` hoists.
  const pk = args.primaryKey && args.primaryKey.length > 0 ? args.primaryKey : pkCols;
  if (pk.length > 0) constraints.push({ kind: { kind: "pk", columns: pk } });

  for (const uq of args.uniques ?? []) {
    constraints.push(compact({ name: uq.name, kind: { kind: "unique", columns: uq.columns } }));
  }
  for (const ck of args.checks ?? []) {
    constraints.push(compact({ name: ck.name, kind: { kind: "check", expr: resolveExpr(ck.expr) } }));
  }
  for (const fkSpec of args.foreignKeys ?? []) {
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
  for (const idx of args.indexes ?? []) {
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

function recordDropTable(
  table: string,
  args: { ifExists?: boolean; cascade?: boolean; schema?: string },
): void {
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

function recordAddColumn(
  table: string,
  column: string,
  type: ColumnDefImpl,
  args: { ifNotExists?: boolean; schema?: string },
): void {
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
  // C2 — `.column(x).add({ type: t.text().unique() })` honors `.unique()`: emit a
  // follow-on unique constraint (mirroring the createTable per-column `.unique()`
  // image, which rides the column's `unique:true` field — but an ADD COLUMN has no
  // inline UNIQUE, so it lowers to a separate ADD CONSTRAINT). Likewise a
  // `.primaryKey()` on an added column hoists a pk add.
  //
  // A PRIMARY KEY already IMPLIES uniqueness, so when BOTH are set the follow-on
  // UNIQUE is redundant DDL (an extra index/constraint) — suppress it when
  // `_primaryKey` is set, mirroring how the differ never emits a separate UNIQUE
  // for the PK column. Only the pk add is recorded.
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

function recordDropColumn(
  table: string,
  column: string,
  args: { ifExists?: boolean; schema?: string },
): void {
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

function recordRenameColumn(
  table: string,
  from: string,
  to: string,
  type: ColumnDefType,
  args: { schema?: string },
): void {
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

function recordAlterColumn(
  table: string,
  name: string,
  change: { type?: ColumnDefType; nullable?: boolean; using?: ExprFn; schema?: string },
): void {
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
function fkConstraintFromSpec(spec: {
  name?: string;
  columns: string[];
  references: ForeignKeyReference;
  onDelete?: RefAction;
  onUpdate?: RefAction;
}): Node {
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

function recordAddForeignKey(
  table: string,
  name: string,
  args: {
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
    ifNotExists?: boolean;
    schema?: string;
  },
): void {
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

function recordAddUnique(
  table: string,
  name: string,
  args: { columns: string[]; ifNotExists?: boolean; schema?: string },
): void {
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

function recordAddCheck(
  table: string,
  name: string,
  args: { expr: ExprFn; ifNotExists?: boolean; schema?: string },
): void {
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

function recordDropConstraint(
  table: string,
  name: string,
  args: { ifExists?: boolean; schema?: string },
): void {
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

function recordCreateIndex(
  table: string,
  name: string,
  args: {
    columns: string[];
    unique?: boolean;
    using?: import("./types.js").IndexMethod;
    where?: ExprFn;
    ifNotExists?: boolean;
    schema?: string;
  },
): void {
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

function recordDropIndex(
  table: string,
  name: string,
  args: { ifExists?: boolean; concurrently?: boolean; unique?: boolean; schema?: string },
): void {
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

function recordInsert<R extends Row = Row>(table: string, args: InsertArgs<R>): void {
  let rows = args.rows as R | R[];
  if (rows === undefined) throw structuredError("OP_INVALID", "insert({ rows }): rows is required");
  if (!Array.isArray(rows)) rows = [rows];
  const arr = rows as R[];
  const columns = arr.length > 0 ? Object.keys(arr[0]) : [];
  const positional = arr.map((r) =>
    columns.map((col) =>
      Object.prototype.hasOwnProperty.call(r, col) ? toIrScalar((r as Row)[col]) : null,
    ),
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

function normalizeOnConflict(
  oc: { columns: string[]; doUpdate?: Record<string, unknown> } | undefined,
): Node | undefined {
  if (oc === undefined) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns } as Node;
  const doUpdate: Record<string, unknown> = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrScalar(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate } as Node;
}

function recordUpdate(table: string, args: UpdateArgs): void {
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

function recordDel(table: string, args: DelArgs): void {
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "del({ where }): where is mandatory");
  }
  push(
    compact({
      op: "delete",
      table,
      where: resolveExpr(args.where),
      limit: args.limit,
      schema: args.schema,
    }),
  );
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

function recordBackfill(table: string, args: BackfillArgs): void {
  if (args.set === undefined) throw structuredError("OP_INVALID", "backfill({ set }): set is required");
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

// ── (E) The fluent `table()` handle — the SOLE public entry (§3) ──

/** Per-op-wins-over-table-default schema precedence (§3/§4): a per-op `schema`
 *  overrides the table default only when the KEY is present with a defined value;
 *  an omitted key (or an explicit `undefined`) keeps the table default. */
function pickSchema(perCall: { schema?: string } | undefined, dflt: string | undefined): string | undefined {
  if (perCall && perCall.schema !== undefined) return perCall.schema;
  return dflt;
}

function requireColumnDef(x: unknown, where: string): asserts x is ColumnDefImpl {
  if (!isColumnDef(x)) {
    throw structuredError("OP_INVALID", `${where} must be a t.* ColumnDef`);
  }
}

export function table(name: string, opts: TableOptions = {}): TableHandle {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;

  const handle: TableHandle = {
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
    column(col): ColumnRef {
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
    foreignKey(fkName): ForeignKeyRef {
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
    unique(uqName): UniqueRef {
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
    check(ckName): CheckRef {
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
    constraint(cName): ConstraintRef {
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
    index(idxName): IndexRef {
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

// ── (C) Determinism lint ──

const NONDETERMINISM_PATTERNS: { re: RegExp; name: string; steer: string }[] = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "c.fn.genRandomUuid() or a DB-evaluated value" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "c.fn.now()" },
];

/**
 * Lint a migration's SOURCE for the nondeterminism accessors (`Date.now()` /
 * `Math.random()` / `crypto.randomUUID()` / `new Date()`).
 *
 * SCOPE — intentional coarse whole-source scan: it OVER-flags (a clock accessor
 * in a comment trips it) and NEVER under-flags. The build-once committed artifact
 * already neutralizes post-deploy non-determinism, so the lint's only job is a
 * best-effort pre-commit STEER where a false positive is cheap (rephrase) and a
 * false negative (a baked build-time value slipping through) is the real hazard.
 * Findings are surfaced as WARNINGS on the record path, never a hard reject.
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
        reason: `${name} bakes a build-time value into the artifact; use the structured FnSynth scalar`,
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
