// `@zeroship/migrate` — the authoring-surface TS types (fluent-only redesign,
// design `2026-06-25-op-dsl-fluent-redesign.md`).
//
// These are MANUAL types that codegen cannot express: the fluent `table()`
// handle + its selector sub-handles (`.column`/`.foreignKey`/…), the chainable
// `ColumnDef` (`t.*`), the `(c) => Expr` `ExprBuilder`, and the all-strings
// typing stance (§3 — names are plain `string`, NOT live-schema-bound). The
// dialect-neutral IR wire types (`Op`, `Expr`, `ColType`, `IrConstraint`, …) are
// GENERATED from the engine's `op-ir.schema.json` (`json-schema-to-typescript`)
// and re-exported from `./generated/ir` AS ERGONOMICS — the goldens remain the
// contract source of truth (§6). This module imports the generated wire types
// where a manual type wants to reference the exact serde shape.

import type {
  Classification,
  ColType,
  CommentTarget,
  Expr,
  ExclusionMethod,
  ExclusionOperator,
  IndexElement,
  IrBatch,
  IrScalar,
  Join,
  JoinKind,
  MaskKind,
  OrderDir,
  OrderItem,
  PolicyCmd,
  RaiseLevel,
  SelectAst,
  SelectItem,
  TableRef,
  TriggerAction,
  TriggerStmt,
  VectorMetric,
  ViewQuery,
} from "./generated/ir.js";

// Re-export the closed facet token unions from the wire layer (single source of
// truth — the IR `ir.ts` mirrors the engine schema; the authoring surface re-exports).
export type {
  ColType,
  CommentTarget,
  Expr,
  ExclusionMethod,
  ExclusionOperator,
  IndexElement,
  IrBatch,
  IrScalar,
  Join,
  JoinKind,
  MaskKind,
  OrderDir,
  OrderItem,
  Classification,
  RaiseLevel,
  SelectAst,
  SelectItem,
  TableRef,
  TriggerAction,
  TriggerStmt,
  VectorMetric,
  ViewQuery,
};

/**
 * **Supported as of op.* PR10 Part B** (executor-side catalog probe). The
 * `ifNotExists` existence guard (the create/add family) is honored by an
 * engine-synthesized catalog probe at apply time: probe the live catalog under the
 * held advisory lock + the open per-step transaction, then `decide` — run the op
 * bare if the object is absent, journal a satisfied no-op if it is already present
 * with the DECLARED shape, or FAIL CLOSED if it is present with a shape that
 * diverges from (or cannot be proven equal to) the declared one. Never a silent
 * skip over a divergence. The option is therefore a plain `boolean`. See
 * `docs/reference/migrate-op-dsl.md` (existence-guard section).
 */
export type IfNotExistsGuard = boolean;

/**
 * **Supported as of op.* PR10 Part B** (executor-side catalog probe). The
 * `ifExists` existence guard (the drop/rename/alter family) is honored by the same
 * probe-under-lock flow: run the drop/alter if the source object is PRESENT,
 * journal a satisfied no-op if it is already absent (a drop has no shape to verify
 * — presence alone governs). A plain `boolean`. See {@link IfNotExistsGuard}.
 */
export type IfExistsGuard = boolean;

// ── Sensitive-data column facets (#173/#174) ──
//
// The closed token unions (`MaskKind` / `Classification` / `VectorMetric`) are
// transcribed in the wire layer (`./generated/ir.ts`, mirroring the engine schema)
// and re-exported above. The OPTION-BAG shapes the authoring `t.*` factories /
// `.mask()` take live here.

/** Options for `t.id({ prefix })` — the typed-id prefix (`usr_<base62>`-style
 *  brand). DECLARED-ONLY: carried on `IrColumn.idPrefix` so the fold / gen-types
 *  keep the brand. Bounded (charset/length/reserved deny-list) by the engine at
 *  validate time. */
export interface IdOptions {
  prefix: string;
}

/** Options for `t.vector(n, { metric })` — the pgvector distance metric (closed
 *  {@link VectorMetric} set). Omitted ⇒ the engine's opclass default. */
export interface VectorOptions {
  metric?: VectorMetric;
}

/** Options for `.mask({ kind, classification? })` — a STANDALONE column mask.
 *  `kind` is REQUIRED (closed {@link MaskKind}); `classification` is optional and
 *  DEFAULTS to `"pii"` (closed {@link Classification}). `kind: "none"` is the
 *  explicit opt-out. */
export interface MaskOptions {
  kind: MaskKind;
  classification?: Classification;
}

/** Options for `.generated(expr, { virtual })`. Omitted ⇒ STORED. */
export interface GeneratedOptions {
  virtual?: boolean;
}

/** Options for `.identity({ always })`. Omitted ⇒ `BY DEFAULT AS IDENTITY`. */
export interface IdentityOptions {
  always?: boolean;
}

// ── The fluent column-type lexicon (`t.*`) → a chainable ColumnDef ──

/**
 * A chainable column definition produced by the fluent `t.*` lexicon (§4).
 * NULLABLE BY DEFAULT; `.notNull()`/`.default(x)`/`.ref(target)`/`.primaryKey()`
 * /`.unique()` opt in. ONE column-type representation — every column-type
 * position (`create.columns`/`.column().add()`/`.column().rename()`/
 * `.column().setType()`) takes a `ColumnDef`.
 *
 * **IMMUTABLE (§4):** every modifier returns a FRESH `ColumnDef` — it does NOT
 * mutate the receiver — so a hoisted type var (`const t1 = t.text().notNull()`)
 * is safe to reuse across multiple columns without aliasing (`t1.unique()` leaves
 * `t1` untouched). This is the contract behind the var-assign authoring style.
 */
export interface ColumnDef {
  /** Mark the column `NOT NULL` (the rarer, riskier opt-in). Returns a fresh def. */
  notNull(): ColumnDef;
  /** A structured default — a typed scalar literal OR a nullary synth scalar
   *  (`{ fn: "now" | "genRandomUuid" }`). NEVER raw SQL (property A). Returns a
   *  fresh def. */
  default(value: ScalarValue | DbSynthSymbol | { fn: "now" | "genRandomUuid" }): ColumnDef;
  /** Re-target as a foreign-key reference (a plain-string target table). Returns
   *  a fresh def. */
  ref(targetTable: string): ColumnDef;
  /** Mark as the table primary key (implies `NOT NULL`). Returns a fresh def. */
  primaryKey(): ColumnDef;
  /** Add a single-column `UNIQUE`. Returns a fresh def. */
  unique(): ColumnDef;
  /**
   * Declare a STANDALONE column mask (#174) — the field reads back as
   * `MaskedValue<T>` and the op lower emits the `__zsmask` sentinel + `_masked`
   * sibling (the same shape `t.encrypted()`'s auto-mask uses). `kind` is REQUIRED
   * (closed {@link MaskKind}); `classification` is optional and DEFAULTS to `"pii"`
   * (closed {@link Classification}). `kind: "none"` is the explicit opt-out. A
   * `.mask()` on an ENCRYPTED column OVERRIDES the auto-mask. Returns a fresh def.
   */
  mask(opts: MaskOptions): ColumnDef;
  /** Declare a generated/computed column from a closed expression AST. Omitted
   * options render a STORED generated column; `{ virtual: true }` requests a
   * SQLite VIRTUAL column and is rejected on Postgres. */
  generated(expr: ExprFn | ExprChain | Expr, opts?: GeneratedOptions): ColumnDef;
  /** Declare a SQL identity column. `{ always: true }` renders
   * `GENERATED ALWAYS AS IDENTITY`; otherwise `BY DEFAULT`. */
  identity(opts?: IdentityOptions): ColumnDef;
}

/** The fluent `t.*` column-type lexicon (shared in shape with `@zeroship/db`).
 *  Canonical names only — the `string`/`int` aliases and the `{notNull,default}`
 *  options-bag overload are REMOVED (§7). */
export interface TypeLexicon {
  /** A conventional id: a non-null UUID PK defaulting to `gen_random_uuid()`.
   *  `t.id({ prefix })` records the typed-id prefix on `IrColumn.idPrefix` so the
   *  fold / gen-types keep the `usr_<base62>`-style brand (declared-only in
   *  `create()` — an added column is never the system PK). */
  id(opts?: IdOptions): ColumnDef;
  text(): ColumnDef;
  /** Fixed-precision decimal (default (38, 9)). */
  numeric(precision?: number, scale?: number): ColumnDef;
  timestamp(): ColumnDef;
  uuid(): ColumnDef;
  bytes(): ColumnDef;
  boolean(): ColumnDef;
  json(): ColumnDef;
  /** A foreign-key reference column (plain-string target — NOT live-schema-bound). */
  ref(targetTable: string): ColumnDef;
  /** A pgvector embedding column of dimensionality `n`. `t.vector(n, { metric })`
   *  records the declared distance metric on `IrColumn.vectorMetric` (the closed
   *  {@link VectorMetric} set), so the ivfflat/hnsw opclass renders the declared
   *  metric instead of defaulting — a declared-only hint introspection can't recover. */
  vector(n: number, opts?: VectorOptions): ColumnDef;
  geoPoint(): ColumnDef;
  /** 32-bit signed integer (canonical; the `int` alias is removed, §7). */
  integer(): ColumnDef;
  int(): ColumnDef;
  bigInt(): ColumnDef;
  float(): ColumnDef;
  /** A named enum reference declared with `enumType(name).create({ values })`. */
  enum(name: string | EnumHandle): ColumnDef;
  /** A named domain reference declared with `domain(name).create(...)` from `@zeroship/migrate/pg`. */
  domain(name: string | DomainHandle): ColumnDef;
  /** An application-level encrypted column wrapping an inner type. */
  encrypted(arg: { of: ColumnDef | ColType } | ColumnDef | ColType): ColumnDef;
}

export interface CreateEnumArgs {
  values: readonly string[];
  schema?: string;
}

export interface DropEnumArgs {
  schema?: string;
  ifExists?: boolean;
}

export interface EnumHandle {
  readonly name: string;
  create(args: CreateEnumArgs): EnumHandle;
  drop(args?: DropEnumArgs): EnumHandle;
  comment(text: string | null, args?: { schema?: string }): EnumHandle;
}

export interface CreateDomainArgs {
  as: ColumnDef | ColType;
  check?: ExprFn | ExprChain | Expr;
  default?: ScalarValue | { fn: "now" | "genRandomUuid" };
  notNull?: boolean;
  schema?: string;
}

export interface DropDomainArgs {
  schema?: string;
  ifExists?: boolean;
}

export interface DomainHandle {
  readonly name: string;
  create(args: CreateDomainArgs): DomainHandle;
  drop(args?: DropDomainArgs): DomainHandle;
  comment(text: string | null, args?: { schema?: string }): DomainHandle;
}

export interface SequenceOwnedBy {
  table: string;
  column: string;
}

export interface CreateSequenceArgs {
  as?: ColumnDef | ColType;
  /** JS-safe signed integer; must be non-zero. */
  increment?: number;
  /** JS-safe signed integer. */
  start?: number;
  /** JS-safe signed integer, or null for NO MINVALUE/default. */
  minValue?: number | null;
  /** JS-safe signed integer, or null for NO MAXVALUE/default. */
  maxValue?: number | null;
  /** Positive JS-safe integer. */
  cache?: number;
  cycle?: boolean;
  ownedBy?: SequenceOwnedBy | null;
  schema?: string;
}

export interface AlterSequenceArgs {
  /** JS-safe signed integer; must be non-zero. */
  increment?: number;
  /** JS-safe signed integer, or null for bare RESTART. */
  restart?: number | null;
  /** JS-safe signed integer, or null for NO MINVALUE/default. */
  minValue?: number | null;
  /** JS-safe signed integer, or null for NO MAXVALUE/default. */
  maxValue?: number | null;
  /** Positive JS-safe integer. */
  cache?: number;
  cycle?: boolean;
  ownedBy?: SequenceOwnedBy | null;
  schema?: string;
}

export interface DropSequenceArgs {
  schema?: string;
  ifExists?: boolean;
}

export interface SequenceHandle {
  readonly name: string;
  create(args?: CreateSequenceArgs): SequenceHandle;
  alter(args: AlterSequenceArgs): SequenceHandle;
  drop(args?: DropSequenceArgs): SequenceHandle;
  comment(text: string | null, args?: { schema?: string }): SequenceHandle;
}

// ── Scalars / rows ──

/** A typed scalar value an `insert` row / default / `onConflict.doUpdate` may
 *  carry (§3.5 numeric domain). The builder normalizes a JS `bigint` into the
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

declare const dbSynthSymbolBrand: unique symbol;

/** Best-effort marker for the native function identities the recorder accepts at
 *  runtime (`Date.now`, `Math.random`, `crypto.randomUUID`). TypeScript cannot
 *  structurally distinguish those symbols from arbitrary nullary functions with
 *  the same return type, so this intentionally permits the native-compatible
 *  signatures for authoring ergonomics. The runtime identity check is the
 *  authoritative guard and fails closed on every other function value. Calls are
 *  intentionally not special: `Date.now()` records the number it returns. */
export type DbSynthSymbol =
  | (() => number)
  | (() => string)
  | { readonly [dbSynthSymbolBrand]: "Date.now" | "Math.random" | "crypto.randomUUID" };

/** A DML value is either a typed scalar or a closed expression node. At runtime,
 *  the exact native function identities above are normalized to
 *  `fnSynth(now/genRandomUuid)`; all other functions are rejected. */
export type DmlValue = ScalarValue | DbSynthSymbol | ExprChain | Expr;

/** A column default value accepted by default-bearing column terminals. */
export type DefaultValue = ScalarValue | DbSynthSymbol | { fn: "now" | "genRandomUuid" };

/** A loose insert row — a `Record<string, DmlValue>`. NEVER auto-bound to the
 *  live schema (§3.5); a caller MAY supply a generic for editor convenience. */
export type Row = Record<string, DmlValue>;

// ── The fluent expression builder (§3.6 / `(c) => Expr`) ──

/** The chainable expression value `c("…")` / every built sub-expression carries.
 *  Each method builds one closed-AST node; bare JS values auto-wrap to `Literal`.
 *  `coalesce`/`concatWs` live ONLY on `c.fn` (§7 c/c.fn dedup) — they are NOT on
 *  the chain. */
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
  // string/value — raw `||` concat only (NULL-skipping joins are `c.fn.concatWs`)
  concat(...parts: unknown[]): ExprChain;
  // null/bool tests
  isNull(): ExprChain;
  isNotNull(): ExprChain;
  isTrue(): ExprChain;
  isFalse(): ExprChain;
  // cast (the closed portable target set only)
  cast(target: "text" | "integer" | "real" | "boolean" | "blob" | "uuid"): ExprChain;
}

/** The `c.fn.*` scalar-function namespace (§3.6) — reached off the single
 *  builder handle; there is no importable `fn`. `coalesce`/`concatWs` live here
 *  (and ONLY here — §7). */
export interface FnNamespace {
  lower(e: unknown): ExprChain;
  upper(e: unknown): ExprChain;
  trim(e: unknown): ExprChain;
  length(e: unknown): ExprChain;
  abs(e: unknown): ExprChain;
  coalesce(...args: unknown[]): ExprChain;
  nullif(a: unknown, b: unknown): ExprChain;
  /** PG vendor scalar for RLS policies: current_setting(name, missing_ok?). */
  currentSetting(name: string, missingOk?: boolean): ExprChain;
  /** PG vendor scalar for RLS policies: current_user. */
  currentUser(): ExprChain;
  /** NULL-skipping safe-join (renders byte-identically on PG/SQLite). */
  concatWs(sep: unknown, ...parts: unknown[]): ExprChain;
  /** The searched `CASE` form. */
  case(branches: [unknown, unknown][], elseVal?: unknown): ExprChain;
  /** The engine-synthesized portable split helper (§9), in-envelope-only. */
  splitPart(col: unknown, delim: string, n: number): ExprChain;
  /** DB-evaluated apply-time scalars, equivalent to the supported bare native
   *  symbols (`Date.now`, `Math.random`, `crypto.randomUUID`). */
  now(): ExprChain;
  genRandomUuid(): ExprChain;
}

/** PostgreSQL-only expression nodes. These methods intentionally live under
 *  `c.pg.*` so the portable chain surface stays dialect-neutral; the Rust
 *  validator rejects these nodes on SQLite/MySQL. */
export interface PgExprNamespace {
  /** Renders `<expr> = ANY (ARRAY['...'::text, ...])` on PostgreSQL. */
  eqAnyArray(expr: unknown, elems: readonly string[]): ExprChain;
  /** Renders `<expr> <> ALL (ARRAY['...'::text, ...])` on PostgreSQL. */
  neAllArray(expr: unknown, elems: readonly string[]): ExprChain;
  /** Renders `<expr> ~ '<pattern>'::text` on PostgreSQL. */
  regex(expr: unknown, pattern: string): ExprChain;
  /** Renders `pg_column_size(<expr>)` on PostgreSQL. */
  columnSize(expr: unknown): ExprChain;
}

/** The single injected builder handle: a column-accessor function `c("name")`
 *  (or `c.col("name")`) carrying the `c.fn.*` namespace. */
export interface ExprBuilder {
  (name: string): ExprChain;
  col(name: string): ExprChain;
  fn: FnNamespace;
  pg: PgExprNamespace;
}

/** An expression position — a `(c) => Expr` callback (the all-strings fluent
 *  form). NEVER a raw string (property A). */
export type ExprFn = (c: ExprBuilder) => ExprChain;

// ── Shared op-arg fragments (§3) ──

/** A FK's `ON DELETE`/`ON UPDATE` referential action (§3.3). Renamed from the old
 *  `FkAction` — these ARE rendered now (C1). */
export type RefAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction";

export type IndexMethod = "btree" | "gin" | "gist" | "ivfflat" | "hnsw" | "fts5";

/** A named foreign-key reference (the `references` half of a FK add / a
 *  `create.foreignKeys[]` entry). */
export interface ForeignKeyReference {
  table: string;
  columns: string[];
}

export interface InsertArgs<R extends Row = Row> {
  rows: R | R[];
  /**
   * **PG-ONLY** upsert. A live, intended feature — rejected as a hard build
   * error only on a SQLite target (`dialect_scope = PgOnly`, §9). There is no
   * portable SQLite upsert and no raw route (property A); a SQLite-targeted
   * `onConflict` surfaces at build with the structured envelope, never at
   * runtime.
   */
  onConflict?: { columns: string[]; doUpdate?: Partial<R> };
  /** The schema qualifier (§3); overrides the handle default. */
  schema?: string;
}

export interface UpdateArgs {
  set: Record<string, ExprFn>;
  where?: ExprFn;
  /** Page a large one-shot UPDATE over a cursor column (`Op::Update.batch`): the
   *  engine lowers it to the same windowed/batched executor a `backfill` uses
   *  (PG writable-CTE windowed UPDATE / SQLite per-batch-txn). Absent ⇒ a single
   *  unbatched UPDATE. Parity with the engine recorder. */
  batch?: IrBatch;
  /** The schema qualifier (§3); overrides the handle default. */
  schema?: string;
}

export interface DelArgs {
  where: ExprFn;
  limit?: number;
  /** The schema qualifier (§3); overrides the handle default. */
  schema?: string;
}

export interface BackfillArgs {
  set: Record<string, ExprFn>;
  where?: ExprFn;
  /** Defaults to the single-column PK (`"id"`). */
  cursorColumn?: string;
  /** Defaults to the engine's chosen batch size. */
  batchSize?: number;
  name?: string;
  /** The schema qualifier (§3); overrides the handle default. */
  schema?: string;
}

export type TriggerTiming = "before" | "after" | "insteadOf";
export type TriggerEvent = "insert" | "update" | "delete" | "truncate";
export type ForEach = "row" | "statement";

export interface TriggerRaiseArgs {
  level: RaiseLevel;
  message: string;
  errcode?: string;
}

export interface TriggerInsertArgs<R extends Row = Row> {
  table: string;
  rows: R | R[];
  schema?: string;
}

export interface TriggerUpdateArgs {
  table: string;
  set: Record<string, ExprFn>;
  where?: ExprFn;
  schema?: string;
}

export interface TriggerDeleteArgs {
  table: string;
  where: ExprFn;
  limit?: number;
  schema?: string;
}

export interface TriggerBodyBuilder {
  raise(args: TriggerRaiseArgs): TriggerStmt;
  insert<R extends Row = Row>(args: TriggerInsertArgs<R>): TriggerStmt;
  update(args: TriggerUpdateArgs): TriggerStmt;
  del(args: TriggerDeleteArgs): TriggerStmt;
  select(expr: ExprFn | ExprChain | Expr): TriggerStmt;
}

interface CreateTriggerBaseArgs {
  name: string;
  timing: TriggerTiming;
  events: TriggerEvent[];
  forEach: ForEach;
  when?: ExprFn;
  schema?: string;
}

export type CreateTriggerArgs =
  | (CreateTriggerBaseArgs & { execute: string; body?: never })
  | (CreateTriggerBaseArgs & { body: (b: TriggerBodyBuilder) => TriggerStmt[]; execute?: never });

export interface DropTriggerArgs {
  name: string;
  ifExists?: boolean;
  schema?: string;
}

export interface CreateTablePolicyArgs {
  name: string;
  for?: PolicyCmd;
  to?: string[];
  using: ExprFn | ExprChain | Expr;
  withCheck?: ExprFn | ExprChain | Expr;
  schema?: string;
}

export interface DropTablePolicyArgs {
  name: string;
  ifExists?: boolean;
  schema?: string;
}

// ── `view()` entry + the closed SelectAst builder (§A1/§3.1) ──

/** The options bag `view(name, opts?)` accepts. Carries the default
 *  `{ schema, columns }` every op the returned {@link ViewHandle} records is
 *  stamped with where applicable. Inline `create({ columns })` /
 *  `createRaw({ columns })` overrides the handle default. */
export interface ViewOptions {
  schema?: string;
  columns?: string[];
}

export type TableRefInput = string | TableRef;
export type SelectProjectionItem = string | SelectItem | ExprFn | ExprChain | Expr;
export type OrderByItem = string | OrderItem | ExprFn | ExprChain | Expr;

export interface ViewQueryBuilder {
  from(table: TableRefInput): ViewQueryBuilder;
  select(items: SelectProjectionItem[]): ViewQueryBuilder;
  join(kind: JoinKind, table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  innerJoin(table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  leftJoin(table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  where(expr: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  orderBy(items: OrderByItem[]): ViewQueryBuilder;
  limit(n: number): ViewQueryBuilder;
}

export interface CreateViewArgs {
  as: ((q: ViewQueryBuilder) => ViewQueryBuilder | SelectAst) | ViewQueryBuilder | SelectAst;
  columns?: string[];
  replace?: boolean;
  materialized?: boolean;
  schema?: string;
}

export interface CreateRawViewArgs {
  sql: string;
  columns?: string[];
  replace?: boolean;
  materialized?: boolean;
  schema?: string;
}

export interface DropViewArgs {
  ifExists?: boolean;
  materialized?: boolean;
  schema?: string;
}

export interface ViewHandle {
  create(args: CreateViewArgs): ViewHandle;
  createRaw(args: CreateRawViewArgs): ViewHandle;
  drop(args?: DropViewArgs): ViewHandle;
  comment(text: string | null, args?: { schema?: string }): ViewHandle;
}

// ── `table()` entry + the fluent handle (§3) ──

/** The options bag `table(name, opts?)` accepts. Carries the default `{ schema }`
 *  every op the returned {@link TableHandle} records is stamped with (a per-op
 *  `schema` override wins; see {@link TableHandle}). An extensible object so a
 *  future per-table default can be added without a signature break. */
export interface TableOptions {
  /** The default schema qualifier propagated to every op the handle records (§3).
   *  Names-are-strings (no live-schema binding); a per-op `schema` on an
   *  individual call overrides it. */
  schema?: string;
}

export type ExclusionTarget = string | ExprFn | ExprChain | Expr;
export type IndexElementArg =
  | string
  | ExprFn
  | ExprChain
  | Expr
  | { kind: "column"; name: string }
  | { kind: "expr"; expr: ExprFn | ExprChain | Expr };

export type CommentTargetArg =
  | { kind: "table"; name: string; schema?: string }
  | { kind: "column"; table: string; name: string; schema?: string }
  | { kind: "index"; name: string; schema?: string }
  | { kind: "constraint"; table: string; name: string; schema?: string }
  | { kind: "view"; name: string; schema?: string }
  | { kind: "type"; name: string; schema?: string }
  | { kind: "sequence"; name: string; schema?: string }
  | { kind: "function"; name: string; schema?: string };

export interface ExclusionElementArg {
  target: ExclusionTarget;
  operator: ExclusionOperator;
}

export interface ExclusionConstraintArgs {
  using?: ExclusionMethod;
  elements: ExclusionElementArg[];
  where?: ExprFn | ExprChain | Expr;
  deferrable?: boolean;
  initiallyDeferred?: boolean;
}

export type ExclusionAddArgs = ExclusionConstraintArgs & {
  ifNotExists?: boolean;
  schema?: string;
};

export type TableStrictness = "strict" | "lenient" | "off";

export interface TableRuntimeOptions {
  softDelete: boolean;
  versioning: boolean;
  strictness?: TableStrictness;
}

export interface SetTableOptionsArgs {
  softDelete?: boolean;
  versioning?: boolean;
  strictness?: TableStrictness;
  schema?: string;
}

/** The all-object `create({...})` payload (§3.1). Table-level constraints/indexes
 *  are FIELDS (no `build` callback — "no exceptions"); each carries a required
 *  `name` (name-first, §3.4).
 *
 *  Apply-level lowering (what reaches the live DDL):
 *  - `uniques`, `foreignKeys`, `indexes` LOWER to DDL on Postgres (a named UNIQUE
 *    + a single-`id` FOREIGN KEY + extra indexes appear in the live catalog).
 *  - `indexes` also lower on SQLite (plain btree); a table-level `uniques` /
 *    `foreignKeys` on SQLite is a HARD authoring error (the SQLite CREATE renders
 *    from the column descriptor — a table-level constraint is not threaded into
 *    the emitter; refused fail-closed rather than silently dropped).
 *  - `foreignKeys` are single-local-column, referencing the target's `id` (the
 *    only shape the renderer emits today); a multi-column / non-`id` FK is a HARD
 *    error (later wave).
 *  - `checks` are HARD deferred errors: the closed-AST expression needs the Wave-C
 *    `Expr`→SQL renderer (same deferral as stand-alone `.check(name).add({expr})`
 *    / `addConstraint(check)`). Partial-index `where` renders on PostgreSQL and
 *    SQLite; MySQL refuses it fail-closed because MySQL has no partial indexes.
 *  - `primaryKey` (composite) and a column's `.primaryKey()` are represented in
 *    the recorded IR. Current confined/platform policy still decides later whether
 *    an authored PK is accepted, rejected, or replaced by the system shape.
 *
 *  None of the above is ever a silent no-op — an unsupported spec fails closed at
 *  lower time. */
export interface CreateTableArgs {
  columns: Record<string, ColumnDef>;
  /** Collection runtime metadata, carried into `schema.runtime.json`.
   *  `softDelete` mirrors `@zeroship/db` `.softDelete()`, `versioning` mirrors
   *  `.withVersioning()`, and `strictness` mirrors `.strictness(...)`. */
  softDelete?: boolean;
  versioning?: boolean;
  strictness?: TableStrictness;
  /** Table primary key intent: undefined leaves the policy default unresolved,
   *  null requests no PK, and a string array records an explicit/composite PK. */
  primaryKey?: string[] | null;
  uniques?: Array<{ name: string; columns: string[] }>;
  /** DEFERRED at apply — the CHECK predicate is a closed-AST `expr` awaiting the
   *  Wave-C `Expr`→SQL renderer; a table-level check is a HARD lower error today
   *  (mirrors stand-alone `.check().add()`), never a silent drop. */
  checks?: Array<{ name: string; expr: ExprFn }>;
  foreignKeys?: Array<{
    name: string;
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
  }>;
  exclusions?: Array<{ name: string } & ExclusionConstraintArgs>;
  indexes?: Array<{
    name: string;
    columns: IndexElementArg[];
    unique?: boolean;
    using?: IndexMethod;
    /** Partial-index predicate. Renders on PostgreSQL and SQLite; MySQL refuses
     *  it fail-closed because MySQL has no partial indexes. */
    where?: ExprFn;
  }>;
  ifNotExists?: boolean;
  /** Overrides the handle default schema. */
  schema?: string;
}

/** The `.column(name)` selector sub-handle (§3.2). Each terminal records eagerly
 *  and returns the parent {@link TableHandle} (so chaining + var-reuse work). */
export interface ColumnRef {
  /** Add the column. Honors ALL modifiers on `type` — including `.unique()` (C2,
   *  emits a follow-on unique constraint) and `.primaryKey()` (emits a follow-on
   *  pk). When BOTH are set, the redundant UNIQUE is suppressed (a PRIMARY KEY
   *  already implies uniqueness) — only the pk add is recorded. */
  add(args: { type: ColumnDef; ifNotExists?: boolean; schema?: string }): TableHandle;
  drop(args?: { ifExists?: boolean; schema?: string }): TableHandle;
  /** Named ⇒ no from/to swap. `type` is the column's type after rename. */
  rename(args: { to: string; type: ColumnDef; schema?: string }): TableHandle;
  setType(args: { to: ColumnDef; using?: ExprFn; schema?: string }): TableHandle;
  setNotNull(args?: { schema?: string }): TableHandle;
  dropNotNull(args?: { schema?: string }): TableHandle;
  setDefault(value: DefaultValue, args?: { schema?: string }): TableHandle;
  dropDefault(args?: { schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
}

/** The `.foreignKey(name)` selector sub-handle (§3.3). */
export interface ForeignKeyRef {
  add(args: {
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
    ifNotExists?: boolean;
    schema?: string;
  }): TableHandle;
}

/** The `.unique(name)` selector sub-handle (§3.3). */
export interface UniqueRef {
  add(args: { columns: string[]; ifNotExists?: boolean; schema?: string }): TableHandle;
}

/** The `.check(name)` selector sub-handle (§3.3). */
export interface CheckRef {
  add(args: { expr: ExprFn; ifNotExists?: boolean; schema?: string }): TableHandle;
}

/** The `.exclusion(name)` selector sub-handle (§3.3). PostgreSQL renders native
 *  `EXCLUDE`; SQLite/MySQL fail closed. */
export interface ExclusionRef {
  add(args: ExclusionAddArgs): TableHandle;
}

/** The `.constraint(name)` selector sub-handle (§3.3) — kind-agnostic drop by
 *  name; its only terminal is `.drop`. */
export interface ConstraintRef {
  drop(args?: { ifExists?: boolean; schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
}

/** The `.index(name)` selector sub-handle (§3.4). */
export interface IndexRef {
  add(args: {
    columns: IndexElementArg[];
    unique?: boolean;
    using?: IndexMethod;
    where?: ExprFn;
    ifNotExists?: boolean;
    schema?: string;
  }): TableHandle;
  /**
   * Drop the index. `unique` is NOT in the spec's literal §3.4 arg list, but the
   * IR `Op::DropIndex.unique` field DRIVES the destructive/approval gating at apply
   * (a `unique: true` drop silently removes a data-integrity guarantee and lowers
   * `destructive + requires_approval`). Keeping it on the surface preserves that
   * apply-path safety signal (the brief's "apply path UNCHANGED"); absent/false ⇒
   * a plain, reversible drop.
   */
  drop(args?: { ifExists?: boolean; concurrently?: boolean; unique?: boolean; schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
}

/**
 * The recorder-bound handle `table(name, opts?)` returns. It is a REUSABLE value
 * carrying only `{ name, schemaDefault }`; every terminal records EAGERLY onto the
 * ambient recorder and returns the handle, so it is valid for unlimited chaining
 * + var-reuse (§4). The
 * `{ schema }` from `table()` is the DEFAULT injected into every recorded op; a
 * per-op `schema` overrides it (by key presence — an absent/`undefined` per-op
 * schema keeps the table default).
 *
 * Direct methods exist for the table itself + its data (§3.1/§3.5); selector
 * sub-handles (`.column`/`.foreignKey`/`.unique`/`.check`/`.constraint`/`.index`)
 * exist for the table's named sub-objects (§3.2/§3.3/§3.4). A selector that is
 * never terminated is a hard `SELECTOR_NOT_TERMINATED` build error at drain (§5).
 */
export interface TableHandle {
  // §3.1 — the table itself (all-object terminals).
  create(args: CreateTableArgs): TableHandle;
  drop(args?: { ifExists?: boolean; cascade?: boolean; schema?: string }): TableHandle;
  /** Rename the whole table to `to` (a fast `ALTER TABLE … RENAME TO …` — NOT the
   *  online column expand-contract; `ifExists` guards the source table). Records a
   *  `renameTable` Op; the engine emits the inverse rename as the down-migration. */
  rename(args: { to: string; ifExists?: boolean; schema?: string }): TableHandle;
  setOptions(args: SetTableOptionsArgs): TableHandle;
  softDelete(enabled?: boolean, args?: { schema?: string }): TableHandle;
  withVersioning(enabled?: boolean, args?: { schema?: string }): TableHandle;
  strictness(level: TableStrictness, args?: { schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;

  // §3.2/§3.3/§3.4 — selectors for named sub-objects
  column(name: string): ColumnRef;
  foreignKey(name: string): ForeignKeyRef;
  unique(name: string): UniqueRef;
  check(name: string): CheckRef;
  exclusion(name: string): ExclusionRef;
  constraint(name: string): ConstraintRef;
  index(name: string): IndexRef;

  // §3.5 — table data (direct named DML; no existence guard — DML is unguardable)
  insert<R extends Row = Row>(args: InsertArgs<R>): TableHandle;
  update(args: UpdateArgs): TableHandle;
  del(args: DelArgs): TableHandle;
  backfill(args: BackfillArgs): TableHandle;

  // `@zeroship/migrate/pg` — table-scoped privileged primitives.
  enableRowLevelSecurity(): TableHandle;
  forceRowLevelSecurity(): TableHandle;
  disableRowLevelSecurity(): TableHandle;
  noForceRowLevelSecurity(): TableHandle;
  createPolicy(args: CreateTablePolicyArgs): TableHandle;
  dropPolicy(args: DropTablePolicyArgs): TableHandle;

  // §A2 — cross-dialect core triggers.
  createTrigger(args: CreateTriggerArgs): TableHandle;
  dropTrigger(args: DropTriggerArgs): TableHandle;
}

/** One determinism-lint finding. */
export interface DeterminismFinding {
  code: "NONDETERMINISTIC_OP_ARG";
  accessor: string;
  suggested_fix: string;
  reason: string;
}

/** The migration module shape (§2): `export default { name?, up, down? }`. */
export interface Migration {
  name?: string;
  up(): void;
  down?(): void;
}
