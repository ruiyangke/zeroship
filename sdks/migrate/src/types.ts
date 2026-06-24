// `@zeroship/migrate` — the authoring-surface TS types (§3.2/§3.3.1).
//
// These are MANUAL types that codegen cannot express: the fluent builder method
// surface (the chainable `ColumnDef` + the `(c) => Expr` `ExprBuilder`), the
// op-function signatures, and the all-strings typing stance (§3.3 — names are
// plain `string`, NOT live-schema-bound). The dialect-neutral IR wire types
// (`Op`, `Expr`, `ColType`, `IrConstraint`, …) are GENERATED from the engine's
// `op-ir.schema.json` (`json-schema-to-typescript`) and re-exported from
// `./generated/ir` AS ERGONOMICS — the goldens remain the contract source of
// truth (§4.3 / PR3). This module imports the generated wire types where a manual
// type wants to reference the exact serde shape.

import type { ColType, Expr, IrBatch, IrScalar } from "./generated/ir.js";

export type { ColType, Expr, IrBatch, IrScalar };

// ── The fluent column-type lexicon (`t.*`) → a chainable ColumnDef ──

/**
 * A chainable column definition produced by the fluent `t.*` lexicon (§3.2).
 * NULLABLE BY DEFAULT; `.notNull()`/`.default(x)`/`.ref(target)`/`.primaryKey()`
 * /`.unique()` opt in. ONE column-type representation — every column-type
 * position (`createTable`/`addColumn`/`renameColumn`/`alterColumn`) takes a
 * `ColumnDef`; there is no second bare-string `ColType` union on the surface.
 */
export interface ColumnDef {
  /** Mark the column `NOT NULL` (the rarer, riskier opt-in). */
  notNull(): ColumnDef;
  /** A structured default — a typed scalar literal OR a nullary synth scalar
   *  (`{ fn: "now" | "genRandomUuid" }`). NEVER raw SQL (property A). */
  default(value: ScalarValue | { fn: "now" | "genRandomUuid" }): ColumnDef;
  /** Re-target as a foreign-key reference (a plain-string target table). */
  ref(targetTable: string): ColumnDef;
  /** Mark as the table primary key (implies `NOT NULL`). */
  primaryKey(): ColumnDef;
  /** Add a single-column `UNIQUE`. */
  unique(): ColumnDef;
}

/** A column-type options-bag overload (`t.text({ notNull: true, default: 0 })`). */
export interface ColumnOpts {
  notNull?: boolean;
  primaryKey?: boolean;
  unique?: boolean;
  ref?: string;
  default?: ScalarValue | { fn: "now" | "genRandomUuid" };
}

/** The fluent `t.*` column-type lexicon (shared in shape with `@zeroship/db`). */
export interface TypeLexicon {
  /** A conventional id: a non-null UUID PK defaulting to `gen_random_uuid()`. */
  id(): ColumnDef;
  text(opts?: ColumnOpts): ColumnDef;
  /** Fixed-precision decimal (default (38, 9)). */
  numeric(precision?: number, scale?: number, opts?: ColumnOpts): ColumnDef;
  timestamp(opts?: ColumnOpts): ColumnDef;
  uuid(opts?: ColumnOpts): ColumnDef;
  bytes(opts?: ColumnOpts): ColumnDef;
  boolean(opts?: ColumnOpts): ColumnDef;
  json(opts?: ColumnOpts): ColumnDef;
  /** A foreign-key reference column (plain-string target — NOT live-schema-bound). */
  ref(targetTable: string, opts?: ColumnOpts): ColumnDef;
  vector(n: number, opts?: ColumnOpts): ColumnDef;
  geoPoint(opts?: ColumnOpts): ColumnDef;
  string(opts?: ColumnOpts): ColumnDef;
  int(opts?: ColumnOpts): ColumnDef;
  integer(opts?: ColumnOpts): ColumnDef;
  bigInt(opts?: ColumnOpts): ColumnDef;
  float(opts?: ColumnOpts): ColumnDef;
  /** An application-level encrypted column wrapping an inner type. */
  encrypted(arg: { of: ColumnDef | ColType } | ColumnDef | ColType, opts?: ColumnOpts): ColumnDef;
}

// ── Scalars / rows ──

/** A typed scalar value an `insert` row / default / `onConflict.doUpdate` may
 *  carry (§2.5/§3.2 numeric domain). The builder normalizes a JS `bigint` into the
 *  `{ decimal }` carrier (integers beyond 2^53) and a `Uint8Array` into the
 *  `{ bytes: base64 }` carrier before recording — both are accepted here for
 *  authoring ergonomics; you MAY also pass the explicit `{ decimal }` carrier (e.g.
 *  for a fractional value). */
export type ScalarValue =
  | string
  | number
  | bigint
  | boolean
  | null
  | { decimal: string }
  | Uint8Array;

/** A loose insert row — a `Record<string, ScalarValue>`. NEVER auto-bound to the
 *  live schema (§3.3); a caller MAY supply a generic for editor convenience. */
export type Row = Record<string, ScalarValue>;

// ── The fluent expression builder (§3.3.1) ──

/** The chainable expression value `c("…")` / every built sub-expression carries.
 *  Each method builds one closed-AST node; bare JS values auto-wrap to `Literal`. */
export interface ExprChain {
  // comparison
  eq(x: unknown): ExprChain;
  ne(x: unknown): ExprChain;
  lt(x: unknown): ExprChain;
  le(x: unknown): ExprChain;
  gt(x: unknown): ExprChain;
  ge(x: unknown): ExprChain;
  // boolean
  and(e: ExprChain): ExprChain;
  or(e: ExprChain): ExprChain;
  not(): ExprChain;
  // arithmetic
  add(x: unknown): ExprChain;
  sub(x: unknown): ExprChain;
  mul(x: unknown): ExprChain;
  div(x: unknown): ExprChain;
  // string/value
  concat(...parts: unknown[]): ExprChain;
  concatWs(sep: unknown, ...parts: unknown[]): ExprChain;
  coalesce(...args: unknown[]): ExprChain;
  // null/bool tests
  isNull(): ExprChain;
  isNotNull(): ExprChain;
  isTrue(): ExprChain;
  isFalse(): ExprChain;
  // cast (the closed portable target set only)
  cast(target: "text" | "integer" | "real" | "boolean" | "blob"): ExprChain;
}

/** The `c.fn.*` scalar-function namespace (§3.3.1) — reached off the single
 *  builder handle; there is no importable `fn`. */
export interface FnNamespace {
  lower(e: unknown): ExprChain;
  upper(e: unknown): ExprChain;
  trim(e: unknown): ExprChain;
  length(e: unknown): ExprChain;
  abs(e: unknown): ExprChain;
  coalesce(...args: unknown[]): ExprChain;
  nullif(a: unknown, b: unknown): ExprChain;
  /** NULL-skipping safe-join (renders byte-identically on PG/SQLite). */
  concatWs(sep: unknown, ...parts: unknown[]): ExprChain;
  /** The searched `CASE` form. */
  case(branches: [unknown, unknown][], elseVal?: unknown): ExprChain;
  /** The engine-synthesized portable split helper (§9), in-envelope-only. */
  splitPart(col: unknown, delim: string, n: number): ExprChain;
  /** DB-evaluated apply-time scalars (the structured replacement for a frozen
   *  `Date.now()` / UUID literal, §4.3). */
  now(): ExprChain;
  genRandomUuid(): ExprChain;
}

/** The single injected builder handle: a column-accessor function `c("name")`
 *  (string arg → a chainable ColRef) carrying the `c.fn.*` namespace. */
export interface ExprBuilder {
  (name: string): ExprChain;
  fn: FnNamespace;
}

/** An expression position — a `(c) => Expr` callback (the all-strings fluent
 *  form). NEVER a raw string (property A). */
export type ExprFn = (c: ExprBuilder) => ExprChain;

// ── Op-function arg shapes (§3.2) ──

export type FkAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction";

export interface ForeignKeySpec {
  columns: string[];
  references: { table: string; columns: string[] };
  onDelete?: FkAction;
  onUpdate?: FkAction;
  name?: string;
}

export interface UniqueSpec {
  columns: string[];
  name?: string;
}

export interface CheckSpec {
  expr: ExprFn;
  name?: string;
}

export interface DropConstraintSpec {
  name: string;
  type?: "pk" | "fk" | "unique" | "check";
  ifExists?: boolean;
}

export type IndexMethod = "btree" | "gin" | "gist" | "ivfflat" | "hnsw" | "fts5";

export interface CreateIndexSpec {
  columns: string[];
  name?: string;
  unique?: boolean;
  using?: IndexMethod;
  where?: ExprFn;
  concurrently?: boolean;
}

export interface AlterColumnChange {
  type?: ColumnDef | ColType;
  using?: ExprFn;
  nullable?: boolean;
}

export interface InsertArgs<R extends Row = Row> {
  rows: R | R[];
  /**
   * **PG-ONLY** upsert. A live, intended feature — rejected as a hard build
   * error only on a SQLite target (`dialect_scope = PgOnly`, §9). There is no
   * portable SQLite upsert and no raw route (property A); a SQLite-targeted
   * `onConflict` surfaces at build with the structured envelope, never at
   * runtime.
   *
   * NOTE: intentionally NOT tagged `@deprecated` — that tag would strike the
   * field through in editor autocomplete and falsely signal removal. `@deprecated`
   * is reserved for genuinely-removed symbols (AGENTS.md: no `@deprecated`
   * aliases for live surface).
   */
  onConflict?: { columns: string[]; doUpdate?: Partial<R> };
}

export interface UpdateArgs {
  set: Record<string, ExprFn>;
  where?: ExprFn;
  /** Page a large one-shot UPDATE over a cursor column (`Op::Update.batch`, §2.3.1):
   *  the engine lowers it to the same windowed/batched executor a `backfill` uses
   *  (PG writable-CTE windowed UPDATE / SQLite per-batch-txn). Absent ⇒ a single
   *  unbatched UPDATE. Parity with the engine recorder. */
  batch?: IrBatch;
}

export interface DelArgs {
  where: ExprFn;
  limit?: number;
}

export interface BackfillArgs {
  set: Record<string, ExprFn>;
  where?: ExprFn;
  /** Defaults to the single-column PK (`"id"`). */
  cursorColumn?: string;
  /** Defaults to the engine's chosen batch size. */
  batchSize?: number;
  name?: string;
}

/** The table-scoped builder `createTable`'s `(b) => …` overload + `batchAlterTable`
 *  pass. */
export interface TableBuilder {
  index(columns: string[], opts?: { name?: string; unique?: boolean; using?: IndexMethod; where?: ExprFn }): void;
  unique(columns: string[], opts?: { name?: string }): void;
  primaryKey(columns: string[]): void;
  check(expr: ExprFn, opts?: { name?: string }): void;
  foreignKey(spec: ForeignKeySpec): void;
}

/** The `batchAlterTable` scoped column/constraint subset (pre-bound to the table). */
export interface BatchAlterBuilder {
  addColumn(name: string, type: ColumnDef): void;
  dropColumn(name: string, opts?: { ifExists?: boolean }): void;
  renameColumn(from: string, to: string, type: ColumnDef): void;
  alterColumn(name: string, change: AlterColumnChange): void;
  addForeignKey(spec: ForeignKeySpec): void;
  addCheck(spec: CheckSpec): void;
}

/** One §4.3 determinism-lint finding. */
export interface DeterminismFinding {
  code: "NONDETERMINISTIC_OP_ARG";
  accessor: string;
  suggested_fix: string;
  reason: string;
}

/** The migration module shape (§3.1): `export default { name?, up, down? }`. */
export interface Migration {
  name?: string;
  up(): void;
  down?(): void;
}
