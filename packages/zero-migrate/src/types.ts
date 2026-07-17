// `zero-migrate` — the authoring-surface TS types (fluent-only).
//
// These are MANUAL types that codegen cannot express: the fluent `table()`
// handle + its selector sub-handles (`.column`/`.foreignKey`/…), the chainable
// `ColumnDef` (`t.*`), the `(col) => Expr` `ExprBuilder`, default-expression
// `DefaultBuilder`, immutable index/generated expression builders, and the all-strings
// typing stance (names are plain `string`, NOT live-schema-bound). The
// dialect-neutral IR wire types (`Op`, `Expr`, `ColType`, `IrConstraint`, …) are
// GENERATED from the engine's `ir-envelope.schema.json` (`json-schema-to-typescript`)
// and re-exported from `./generated/ir` AS ERGONOMICS — the goldens remain the
// contract source of truth. This module imports the generated wire types
// where a manual type wants to reference the exact serde shape.

import type {
  Classification,
  CastTarget,
  ColType,
  CommentTarget,
  Expr,
  ExtractField,
  ExclusionMethod,
  ExclusionOperator,
  IndexElement,
  IndexSortOrder,
  IndexStorageParams,
  IrBatch,
  IrJsonValue,
  IrScalar,
  Join,
  JoinKind,
  MaskKind,
  OrderDir,
  OrderItem,
  PartitionBoundValue,
  PartitionBounds,
  PartitionSpec,
  PerRowGenerator,
  PgExtractField,
  PolicyCmd,
  RaiseLevel,
  SelectAst,
  SelectItem,
  SequenceRef,
  TableRef,
  TriggerAction,
  TriggerStmt,
  ValueFormat,
  VectorMetric,
  ViewQuery,
} from "./generated/ir.js";

// Re-export the closed facet token unions from the wire layer (single source of
// truth — the IR `ir.ts` mirrors the engine schema; the authoring surface re-exports).
export type {
  CastTarget,
  ColType,
  CommentTarget,
  Expr,
  ExtractField,
  ExclusionMethod,
  ExclusionOperator,
  IndexElement,
  IndexSortOrder,
  IndexStorageParams,
  IrBatch,
  IrJsonValue,
  IrScalar,
  Join,
  JoinKind,
  MaskKind,
  OrderDir,
  OrderItem,
  PartitionBoundValue,
  PartitionBounds,
  PartitionSpec,
  PerRowGenerator,
  PgExtractField,
  Classification,
  RaiseLevel,
  SelectAst,
  SelectItem,
  SequenceRef,
  TableRef,
  TriggerAction,
  TriggerStmt,
  ValueFormat,
  VectorMetric,
  ViewQuery,
};

// Existence guards (`ifNotExists` / `ifExists`) are inline boolean options on the
// relevant op specs. The executor honors them via catalog probes under lock; see
// `docs/writing-migrations.md` (existence-guard section).

// ── Sensitive-data column facets (#173/#174) ──
//
// The closed token unions (`MaskKind` / `Classification` / `VectorMetric`) are
// transcribed in the wire layer (`./generated/ir.ts`, mirroring the engine schema)
// and re-exported above. The OPTION-BAG shapes the authoring `t.*` factories /
// `.mask()` take live here.

/** Options for the published TypeID 0.3 text format. The empty prefix is
 * valid and stores the 26-character suffix without a leading underscore. */
export interface TypeIdOptions {
  prefix: string;
}

/** First-class, validated textual ID formats. These builders select storage
 * and validation only; they remain nullable and carry no generator or key
 * semantics unless the ordinary {@link ColumnDef} modifiers opt in. */
export interface IdFormats {
  typeId(options: TypeIdOptions): ColumnDef;
  ulid(): ColumnDef;
}

declare const perRowGeneratorBrand: unique symbol;

/** An apply-engine generator intent. It is accepted only as a value in
 * `backfill({ set })`; it is not a scalar, SQL expression, column default, or
 * application-runtime ID generator. */
export interface PerRowGeneratorValue {
  readonly [perRowGeneratorBrand]: "perRowGenerator";
}

/** Apply-engine generators evaluated independently for every affected row of a
 * migration backfill. */
export interface PerRowGenerators {
  uuidV4(): PerRowGeneratorValue;
  uuidV7(): PerRowGeneratorValue;
  typeId(options: TypeIdOptions): PerRowGeneratorValue;
  ulid(): PerRowGeneratorValue;
}

/** Options for `t.text({ caseSensitive })`. `false` records the portable
 *  case-insensitive-text facet; omitted/`true` emits no facet. */
export interface TextOptions {
  caseSensitive?: boolean;
}

/** Options for `t.numeric({ precision, scale })`. Omitted ⇒ (38, 9). */
export interface NumericOptions {
  precision?: number;
  scale?: number;
}

/** Options for `t.char({ length })`. */
export interface CharOptions {
  length: number;
}

/** Options for `t.vector({ dimensions, metric })` — the pgvector dimensionality
 *  plus optional distance metric (closed {@link VectorMetric} set). Omitted
 *  metric ⇒ the engine's opclass default. */
export interface VectorOptions {
  dimensions: number;
  metric?: VectorMetric;
}

/** Structured duration value for PG interval literals and future date shifting.
 *  At least one integer field is required; absent fields are omitted on the wire
 *  in canonical order (`years`, `months`, `days`, `hours`, `minutes`, `seconds`). */
export interface Duration {
  years?: number;
  months?: number;
  days?: number;
  hours?: number;
  minutes?: number;
  seconds?: number;
}

export interface CurrentSettingOptions {
  missingOk?: boolean;
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

export interface NextvalOptions {
  schema?: string;
}

declare const nextvalDefaultBrand: unique symbol;

export interface NextvalDefault {
  readonly [nextvalDefaultBrand]: "nextval";
  readonly name: string;
  readonly schema?: string;
}

// ── The fluent column-type lexicon (`t.*`) → a chainable ColumnDef ──

/**
 * A chainable column definition produced by the fluent `t.*` lexicon.
 * NULLABLE BY DEFAULT; `.notNull()`/`.default(x)`/`.primaryKey()`/`.unique()`
 * opt in. ONE column-type representation — every column-type
 * position (`create.columns`/`.column().add()`/`.column().rename()`/
 * `.column().setType()`) takes a `ColumnDef`.
 *
 * **IMMUTABLE:** every modifier returns a FRESH `ColumnDef` — it does NOT
 * mutate the receiver — so a hoisted type var (`const t1 = t.text().notNull()`)
 * is safe to reuse across multiple columns without aliasing (`t1.unique()` leaves
 * `t1` untouched). This is the contract behind the var-assign authoring style.
 */
export interface ColumnDef {
  /** Mark the column `NOT NULL` (the rarer, riskier opt-in). Returns a fresh def. */
  notNull(): ColumnDef;
  /** A structured default — a typed scalar/container literal, `nextval(...)`, a
   *  top-level value constructor (`now()`, `uuidV4()`, `uuidV7()`), or a narrow
   *  expression callback. NEVER raw SQL (property A).
   *  Returns a fresh def. */
  default(value: DefaultValue | DefaultExprFn | ExprChain | Expr): ColumnDef;
  /** Mark as the table primary key (implies `NOT NULL`). Returns a fresh def. */
  primaryKey(): ColumnDef;
  /** Add a single-column `UNIQUE`. Returns a fresh def. */
  unique(): ColumnDef;
  /**
   * Declare a typed single-column foreign-key reference. The local physical
   * type and value-format facets remain exactly those selected by this builder;
   * the reference adds only the target and referential actions. Returns a fresh
   * def. References are currently create-table-only; add/rename/set-type and
   * nested type positions reject this facet instead of dropping it.
   */
  references(
    table: string,
    column: string,
    options?: {
      onDelete?: RefAction;
      onUpdate?: RefAction;
    },
  ): ColumnDef;
  /**
   * Declare a STANDALONE column mask (#174) — the field reads back as
   * `MaskedValue<T>` and the op lower emits the `zero-migrate:mask` sentinel + `_masked`
   * sibling (the same shape `t.encrypted()`'s auto-mask uses). `kind` is REQUIRED
   * (closed {@link MaskKind}); `classification` is optional and DEFAULTS to `"pii"`
   * (closed {@link Classification}). `kind: "none"` is the explicit opt-out. A
   * `.mask()` on an ENCRYPTED column OVERRIDES the auto-mask. Returns a fresh def.
   */
  mask(opts: MaskOptions): ColumnDef;
  /** Declare a generated/computed column from a closed expression AST. Omitted
   * options render a STORED generated column; `{ virtual: true }` requests a
   * SQLite VIRTUAL column and is rejected on Postgres. */
  generated(expr: GeneratedColumnExprFn | ExprChain | Expr, opts?: GeneratedOptions): ColumnDef;
  /** Declare a SQL identity column. `{ always: true }` renders
   * `GENERATED ALWAYS AS IDENTITY`; otherwise `BY DEFAULT`. */
  identity(opts?: IdentityOptions): ColumnDef;
  /** Portable auto-increment intent for integer columns. Sugar for
   * `.identity({ always: false })`. */
  autoIncrement(): ColumnDef;
}

/** The fluent migration `t.*` physical column-type lexicon. Canonical names only
 * — the universal ID shortcut, untyped reference factory, `string`/`integer`
 * aliases, and the `{notNull,default}` options-bag overload are removed. */
export interface TypeLexicon {
  text(opts?: TextOptions): ColumnDef;
  /** PostgreSQL `text[]` column. Non-PG backends store the array payload as JSON text. */
  textArray(): ColumnDef;
  /** Fixed-precision decimal (default (38, 9)). */
  numeric(opts?: NumericOptions): ColumnDef;
  /** Fixed-length character string (`character(n)` / `CHAR(n)`). */
  char(opts: CharOptions): ColumnDef;
  timestamp(): ColumnDef;
  /** Portable SQL DATE: PostgreSQL `date`, MySQL `DATE`, SQLite `TEXT` date affinity. */
  date(): ColumnDef;
  uuid(): ColumnDef;
  bytes(): ColumnDef;
  boolean(): ColumnDef;
  json(): ColumnDef;
  /** A pgvector embedding column. `t.vector({ dimensions, metric })` records the
   *  declared distance metric on `IrColumn.vectorMetric` (the closed
   *  {@link VectorMetric} set), so the ivfflat/hnsw opclass renders the declared
   *  metric instead of defaulting — a declared-only hint introspection can't recover. */
  vector(opts: VectorOptions): ColumnDef;
  geoPoint(): ColumnDef;
  /** 16-bit signed integer. */
  smallInt(): ColumnDef;
  /** 32-bit signed integer (canonical; `t.integer` is deleted). */
  int(): ColumnDef;
  bigInt(): ColumnDef;
  /** Single-precision float (float4). */
  real(): ColumnDef;
  /** Double-precision float (float8). */
  double(): ColumnDef;
  /** IP network/address (`inet` on Postgres). */
  inet(): ColumnDef;
  /** A named enum reference declared with `enumType(name).create({ values })`. */
  enum(name: string | EnumHandle): ColumnDef;
  /** A named domain reference declared with `domain(name).create(...)` from `zero-migrate`. */
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
  check?: DomainCheckFn | ExprChain | Expr;
  default?: ScalarValue | DefaultExprFn;
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

export interface SchemaCreateArgs {
  ifNotExists?: boolean;
  authorization?: string;
}

export interface SchemaDropArgs {
  ifExists?: boolean;
  cascade?: boolean;
}

export interface DroppedSchemaHandle {
  readonly name: string;
  create(args?: SchemaCreateArgs): SchemaHandle;
}

export interface SchemaHandle extends DroppedSchemaHandle {
  drop(args?: SchemaDropArgs): DroppedSchemaHandle;
}

export interface ExtensionCreateArgs {
  ifNotExists?: boolean;
  schema?: string;
}

export interface ExtensionDropArgs {
  ifExists?: boolean;
}

export interface DroppedExtensionHandle {
  readonly name: string;
  create(args?: ExtensionCreateArgs): ExtensionHandle;
}

export interface ExtensionHandle extends DroppedExtensionHandle {
  drop(args?: ExtensionDropArgs): DroppedExtensionHandle;
}

export interface RoleCreateArgs {
  login?: boolean;
  password?: string;
  bypassRls?: boolean;
  createRole?: boolean;
  createDb?: boolean;
  superuser?: boolean;
  inRole?: string[];
  setSearchPath?: string[];
  ifNotExists?: boolean;
}

export interface RoleSetOptionsArgs {
  setSearchPath?: string[];
  resetSearchPath?: boolean;
}

export interface RoleDropArgs {
  ifExists?: boolean;
}

export interface DroppedRoleHandle {
  readonly name: string;
  create(args?: RoleCreateArgs): RoleHandle;
}

export interface RoleHandle extends DroppedRoleHandle {
  setOptions(args: RoleSetOptionsArgs): RoleHandle;
  drop(args?: RoleDropArgs): DroppedRoleHandle;
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

declare const decimalValueBrand: unique symbol;
declare const bytesValueBrand: unique symbol;
declare const int64ValueBrand: unique symbol;

/** A branded decimal value produced by the top-level `decimal("...")`
 *  constructor. The recorder normalizes it to the IR `{ decimal: "..." }`
 *  scalar carrier; authors cannot pass the in-band carrier directly. */
export interface DecimalValue {
  readonly [decimalValueBrand]: "decimal";
  readonly decimal: string;
}

/** A branded bytes value produced by the top-level `byteValue(...)`
 *  constructor. The recorder normalizes it to the IR `{ bytes: "<base64>" }`
 *  scalar carrier; raw `Uint8Array` values remain accepted. */
export interface BytesValue {
  readonly [bytesValueBrand]: "bytes";
  readonly bytes: string;
}

/** An exact signed 64-bit integer produced by the top-level `int64(...)`
 *  constructor. The recorder normalizes it to the IR `{ int64: "..." }`
 *  scalar carrier; raw JavaScript `bigint` values remain rejected. */
export interface Int64Value {
  readonly [int64ValueBrand]: "int64";
  readonly int64: string;
}

/** The pinned scalar set accepted by value-list predicates such as
 *  `.in([...])` / `.notIn([...])`. */
export type Scalar = string | number | boolean | null;

/** A typed scalar value an `insert` row / default / `onConflict.doUpdate` may
 *  carry (numeric / bytes domain). The builder normalizes branded `int64(...)`,
 *  `decimal(...)`, and `byteValue(...)` values into their tagged IR carriers,
 *  and a `Uint8Array` into the `{ bytes: base64 }` carrier before recording. */
export type ScalarValue =
  | Scalar
  | Int64Value
  | DecimalValue
  | BytesValue
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
 *  the exact native function identities above are normalized to `fnSynth(now)`
 *  or the exact `uuidV4` expression; all other functions are rejected. */
export type DmlValue = ScalarValue | DbSynthSymbol | ExprChain | Expr;

/** A DML assignment RHS accepts the same scalar/expression values as insert rows,
 *  plus the `(col) => Expr` callback shorthand. */
export type DmlSetValue = DmlValue | ExprFn;

/** A backfill assignment additionally admits an apply-engine per-row generator.
 * Ordinary insert/update/default positions intentionally keep their narrower
 * value types. */
export type BackfillSetValue = DmlSetValue | PerRowGeneratorValue;

/** Expression-position dialect legs. Missing own leg with no default is refused
 *  by the engine for that target. */
export type DialectExprLegs = {
  default?: unknown;
  pg?: unknown;
  sqlite?: unknown;
  mysql?: unknown;
};

/** Op-position dialect legs. Each present leg is thunked and records normal ops;
 *  missing own leg with no default is skipped for that target. */
export type DialectOpLegs = {
  default?: () => void;
  pg?: () => void;
  sqlite?: () => void;
  mysql?: () => void;
};

/** Empty object/array defaults admitted for JSON/text-array columns. */
export type EmptyContainerDefault = Record<string, never> | readonly [];

export type JsonDefaultObject = {
  readonly [key: string]: JsonDefaultValue;
} & {
  readonly fn?: never;
};

/** JSON value defaults admitted for JSON columns. Runtime recording accepts only
 *  integer JS numbers (`Number.isInteger(v) && Math.abs(v) < 2**53`) in v1. */
export type JsonDefaultValue =
  | null
  | boolean
  | string
  | number
  | readonly JsonDefaultValue[]
  | JsonDefaultObject;

/** A column default value accepted by default-bearing column terminals. */
export type DefaultValue =
  | ScalarValue
  | NextvalDefault
  | EmptyContainerDefault
  | JsonDefaultValue;

export declare function int64(value: bigint | string): Int64Value;
export declare function now(): ExprChain;
/** Database-evaluated RFC 9562 UUIDv4 expression. */
export declare function uuidV4(): ExprChain;
/** Database-evaluated RFC 9562 UUIDv7 expression. Unsupported targets or server versions fail validation or apply. */
export declare function uuidV7(): ExprChain;
/** @deprecated Use {@link uuidV4} instead. */
export declare function genRandomUuid(): ExprChain;
export declare function currentSetting(name: string, opts?: CurrentSettingOptions): ExprChain;
export declare function currentUser(): ExprChain;
export declare function interval(duration: Duration): ExprChain;
export declare function concatWs(sep: unknown, ...parts: unknown[]): ExprChain;
export declare function countStar(): ExprChain;

/** A loose insert row — a `Record<string, DmlValue>`. NEVER auto-bound to the
 *  live schema; a caller MAY supply a generic for editor convenience. */
export type Row = Record<string, DmlValue>;

// ── The fluent expression builder (`(col) => Expr`) ──

/** The chainable expression value `col("…")` / every built sub-expression carries.
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
  and(...es: ExprChain[]): ExprChain;
  or(...es: ExprChain[]): ExprChain;
  not(): ExprChain;
  // arithmetic
  add(x: unknown): ExprChain;
  sub(x: unknown): ExprChain;
  mul(x: unknown): ExprChain;
  div(x: unknown): ExprChain;
  // string/value — raw `||` concat only (NULL-skipping joins are `concatWs`)
  concat(...parts: unknown[]): ExprChain;
  // null/bool tests
  isNull(): ExprChain;
  isNotNull(): ExprChain;
  isTrue(): ExprChain;
  isFalse(): ExprChain;
  // cast (the closed scalar ColType target set only)
  cast(args: { to: CastTarget }): ExprChain;
  // portable predicates: `between`/`like` render identical syntax on all
  // three dialects; `in`/`notIn` are portably named but keep PG's pg_dump-faithful
  // `ANY/ALL ARRAY[...]` render for homogeneous scalar lists; `distinctFrom` is
  // portably named but per-dialect rendered (PG/SQLite `IS DISTINCT FROM` vs
  // MySQL `NOT (x <=> y)`).
  between(low: unknown, high: unknown): ExprChain;
  like(pattern: unknown): ExprChain;
  "in"(values: readonly Scalar[]): ExprChain;
  notIn(values: readonly Scalar[]): ExprChain;
  distinctFrom(x: unknown): ExprChain;
  // PostgreSQL-first chain operators. First-class on the core surface — no
  // `/pg` import, no `col.pg.` cast. The Rust validator fails closed on targets
  // that lack a native form (`regex`: `~` on PG, `REGEXP` on MySQL, error on
  // SQLite; `columnSize`: `pg_column_size` on PG, error elsewhere); use
  // `dialect({...})` to supply a portable leg.
  /** `<expr> ~ '<pattern>'` (PG) / `<expr> REGEXP '<pattern>'` (MySQL). */
  regex(pattern: string): ExprChain;
  /** `pg_column_size(<expr>)` — PostgreSQL on-disk byte size of the value. */
  columnSize(): ExprChain;
  // scalar functions — receiver-first authoring for the fnCall/fnSynth nodes.
  lower(): ExprChain;
  upper(): ExprChain;
  trim(): ExprChain;
  length(): ExprChain;
  abs(): ExprChain;
  coalesce(...rest: unknown[]): ExprChain;
  nullif(b: unknown): ExprChain;
  /** Integer/numeric modulo, `(a % b)` — portable (`%` on PG/SQLite/MySQL). */
  mod(b: unknown): ExprChain;
  /** `round(x)` / `round(x, n)` — portable rounding, optional precision. */
  round(n?: unknown): ExprChain;
  /** `floor(x)` — portable floor (PG/MySQL/SQLite≥3.35). */
  floor(): ExprChain;
  /** `ceil(x)` — portable ceiling (PG/SQLite≥3.35/MySQL). */
  ceil(): ExprChain;
  /** `substr(s, start[, len])` — portable substring, 1-based start. */
  substr(start: unknown, len?: unknown): ExprChain;
  /** `replace(s, from, to)` — portable string replace. */
  replace(from: unknown, to: unknown): ExprChain;
  /** Date/time part extraction. PG-only fields record pgExtract and validate fail-closed off-PG. */
  extract(field: ExtractField | PgExtractField): ExprChain;
  /** The engine-synthesized portable split helper, in-envelope-only. */
  splitPart(delim: string, n: number): ExprChain;
  // aggregate nodes: receiver-first authoring for COUNT(expr),
  // SUM/AVG/MIN/MAX plus PG-first stringAgg/arrayAgg/boolAnd/boolOr.
  // Receiver-less COUNT(*) is the top-level countStar() import.
  count(opts?: { distinct?: boolean }): ExprChain;
  sum(opts?: { distinct?: boolean }): ExprChain;
  avg(opts?: { distinct?: boolean }): ExprChain;
  min(opts?: { distinct?: boolean }): ExprChain;
  max(opts?: { distinct?: boolean }): ExprChain;
  /** `string_agg(<expr>, <delimiter>)` — PostgreSQL-first; use `dialect({...})` to port. */
  stringAgg(delimiter: unknown): ExprChain;
  /** `array_agg(<expr>)` — PostgreSQL-first; use `dialect({...})` to port. */
  arrayAgg(): ExprChain;
  /** `bool_and(<expr>)` — PostgreSQL-first; use `dialect({...})` to port. */
  boolAnd(): ExprChain;
  /** `bool_or(<expr>)` — PostgreSQL-first; use `dialect({...})` to port. */
  boolOr(): ExprChain;
}

/** The deliberately narrow builder available in column default expression
 *  callbacks. Defaults cannot reference columns, vendor PG helpers, or trigger
 *  OLD/NEW state by construction. Aggregates are type-reachable through chain
 *  methods / `countStar()` and rejected by Rust validation. Receiver-less value
 *  constructors are top-level imports (`now()`, `uuidV4()`, `uuidV7()`). */
export interface DefaultBuilder {
  /** The searched `CASE` form: `col.case({ branches: [{ when, then }], else? })`. */
  case(args: { branches: Array<{ when: unknown; then: unknown }>; else?: unknown }): ExprChain;
}

/** A column default expression callback. */
export type DefaultExprFn = (col: DefaultBuilder) => ExprChain | Expr;

interface ImmutableExprBuilderBase {
  (name: string): ExprChain;
  (table: string, name: string): ExprChain;
  /** The searched `CASE` form: `col.case({ branches: [{ when, then }], else? })`. */
  case(args: { branches: Array<{ when: unknown; then: unknown }>; else?: unknown }): ExprChain;
}

/** Builder for index expressions and predicates. Column refs, expression chain
 *  methods, and `col.case(...)` are available; volatile functions and aggregates
 *  are rejected by Rust validation in these scalar contexts. Trigger OLD/NEW
 *  state is not exposed. */
export interface IndexExprBuilder extends ImmutableExprBuilderBase {}

/** Builder for generated column expressions. Structurally matches
 *  `IndexExprBuilder` today but remains distinct so the slots can diverge. */
export interface GeneratedColumnBuilder extends ImmutableExprBuilderBase {}

/** Builder for portable table CHECK expressions. CHECKs must be immutable and
 *  non-aggregate at validate time. PG-only expression nodes fail closed off-PG. */
export interface CheckBuilder extends ImmutableExprBuilderBase {}

export type IndexExprFn = (col: IndexExprBuilder) => ExprChain | Expr;
export type GeneratedColumnExprFn = (col: GeneratedColumnBuilder) => ExprChain | Expr;
export type CheckExprFn = (col: CheckBuilder) => ExprChain | Expr;

/** The single injected builder handle: a column-accessor function `col("name")`
 *  carrying only `case`. A two-arg form
 *  `col("table", "col")` produces a qualified colRef (the join-ON fix). */
export interface ExprBuilder {
  (name: string): ExprChain;
  (table: string, name: string): ExprChain;
  /** The searched `CASE` form: `col.case({ branches: [{ when, then }], else? })`. */
  case(args: { branches: Array<{ when: unknown; then: unknown }>; else?: unknown }): ExprChain;
}

/** An expression position — a `(col) => Expr` callback (the all-strings fluent
 *  form). NEVER a raw string (property A). */
export type ExprFn = (col: ExprBuilder) => ExprChain;

/** A named table-level CHECK constraint authored inside `table().create({ checks })`. */
export interface CheckDef {
  name: string;
  expr: CheckExprFn;
}

/** Builder for PostgreSQL domain CHECK expressions. The handle itself is the
 *  domain VALUE expression; there is no general column accessor because a domain
 *  CHECK can only refer to the value being constrained. */
export interface DomainValueBuilder extends ExprChain {
  /** The searched `CASE` form: `v.case({ branches: [{ when, then }], else? })`. */
  case(args: { branches: Array<{ when: unknown; then: unknown }>; else?: unknown }): ExprChain;
}

export type DomainCheckFn = (v: DomainValueBuilder) => ExprChain | Expr;

// ── Shared op-arg fragments ──

/** A FK's `ON DELETE`/`ON UPDATE` referential action. Renamed from the old
 *  `FkAction` — these ARE rendered now. */
export type RefAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction";

export type IndexMethod = "btree" | "hash" | "gin" | "gist" | "spgist" | "brin" | "ivfflat" | "hnsw" | "fts5";

export interface PartitionBoundSentinel {
  readonly __zeroMigratePartitionBound: "minValue" | "maxValue";
}

export type PartitionBoundInput = string | number | PartitionBoundSentinel;

export type PartitionByInput =
  | { range: readonly string[]; whenUnsupported?: "collapse" }
  | { list: readonly string[]; whenUnsupported?: "collapse" }
  | { hash: readonly string[]; whenUnsupported?: "collapse" };

export interface CreatePartitionOptions {
  schema?: string;
  ifNotExists?: boolean;
}

export type PartitionBoundArgs =
  | { from: readonly PartitionBoundInput[]; to: readonly PartitionBoundInput[] }
  | { in: readonly PartitionBoundInput[] }
  | { modulus: number; remainder: number }
  | { default: true };

export interface PartitionRef {
  create(bound: PartitionBoundArgs, args?: CreatePartitionOptions): TableHandle;
  attach(bound: PartitionBoundArgs, args?: AttachPartitionArgs): TableHandle;
  drop(args?: DropPartitionArgs): TableHandle;
  detach(args?: DetachPartitionArgs): TableHandle;
}

export interface DropPartitionArgs {
  schema?: string;
  ifExists?: boolean;
  cascade?: boolean;
}

export interface DetachPartitionArgs {
  schema?: string;
  concurrently?: boolean;
}

export interface AttachPartitionArgs {
  schema?: string;
}

export type IndexStorageParamsArg = IndexStorageParams;

/** A named foreign-key reference (the `references` half of a FK add / a
 *  `create.foreignKeys[]` entry). */
export interface ForeignKeyReference {
  /** Optional authoring hint. The frozen IR carries the table name only, so this
   *  must match the table/op schema when provided. */
  schema?: string;
  table: string;
  columns: string[];
}

export interface InsertArgs<R extends Row = Row> {
  rows: R | R[];
  /**
   * Structured upsert. PostgreSQL and SQLite use the exact conflict target.
   * MySQL 8 supports a non-empty `doUpdate` through native duplicate-key handling
   * after proving the target exactly matches one full primary or unique index.
   * On MySQL, include every target column in `rows`, do not assign a target column
   * from `doUpdate`, and do not omit `doUpdate`; targeted do-nothing has no exact
   * MySQL form.
   */
  onConflict?: { columns: string[]; doUpdate?: Record<string, DmlValue> };
  /** The schema qualifier; overrides the handle default. */
  schema?: string;
}

export interface UpdateArgs {
  set: Record<string, DmlSetValue>;
  where?: ExprFn | ExprChain | Expr;
  /** The schema qualifier; overrides the handle default. */
  schema?: string;
}

export interface DelArgs {
  where: ExprFn | ExprChain | Expr;
  limit?: number;
  /** The schema qualifier; overrides the handle default. */
  schema?: string;
}

export interface BackfillArgs {
  set: Record<string, BackfillSetValue>;
  where?: ExprFn | ExprChain | Expr;
  /** Defaults to the single-column PK (`"id"`). */
  cursorColumn?: string;
  /** Defaults to the engine's chosen batch size. */
  batchSize?: number;
  name?: string;
  /** The schema qualifier; overrides the handle default. */
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
  set: Record<string, DmlSetValue>;
  where?: ExprFn | ExprChain | Expr;
  schema?: string;
}

export interface TriggerDeleteArgs {
  table: string;
  where: ExprFn | ExprChain | Expr;
  limit?: number;
  schema?: string;
}

export interface TriggerBodyBuilder {
  raise(args: TriggerRaiseArgs): TriggerStmt;
  insert<R extends Row = Row>(args: TriggerInsertArgs<R>): TriggerStmt;
  update(args: TriggerUpdateArgs): TriggerStmt;
  delete(args: TriggerDeleteArgs): TriggerStmt;
  select(expr: ExprFn | ExprChain | Expr): TriggerStmt;
}

interface TriggerCreateBaseArgs {
  timing: TriggerTiming;
  events: TriggerEvent[];
  forEach: ForEach;
  when?: ExprFn;
  schema?: string;
}

export type TriggerCreateArgs =
  | (TriggerCreateBaseArgs & { execute: string; body?: never })
  | (TriggerCreateBaseArgs & { body: (b: TriggerBodyBuilder) => TriggerStmt[]; execute?: never });

export interface TriggerDropArgs {
  ifExists?: boolean;
  schema?: string;
}

export interface TriggerRef {
  create(args: TriggerCreateArgs): TableHandle;
  drop(args?: TriggerDropArgs): TableHandle;
}

export interface PolicyCreateArgs {
  for?: PolicyCmd;
  to?: string[];
  using: ExprFn | ExprChain | Expr;
  withCheck?: ExprFn | ExprChain | Expr;
  schema?: string;
}

export interface PolicyDropArgs {
  ifExists?: boolean;
  schema?: string;
}

export interface PolicyRef {
  create(args: PolicyCreateArgs): TableHandle;
  drop(args?: PolicyDropArgs): TableHandle;
}

// ── `view()` entry + the closed SelectAst builder ──

/** The options bag `view(name, opts?)` accepts. Carries the default
 *  `{ schema, columns }` every op the returned {@link ViewHandle} records is
 *  stamped with where applicable. Inline `create({ columns })` /
 *  `create({ columns })` overrides the handle default. */
export interface ViewOptions {
  schema?: string;
  columns?: string[];
}

export type TableRefInput = string | TableRef;
export type SelectProjectionItem = string | SelectItem | ExprFn | ExprChain | Expr;
export type GroupByItem = string | ExprFn | ExprChain | Expr;
export type OrderByItem = string | OrderItem | ExprFn | ExprChain | Expr;

export interface ViewQueryBuilder {
  from(table: TableRefInput): ViewQueryBuilder;
  select(items: SelectProjectionItem[]): ViewQueryBuilder;
  join(kind: JoinKind, table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  innerJoin(table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  leftJoin(table: TableRefInput, on: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  where(expr: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  groupBy(items: GroupByItem[]): ViewQueryBuilder;
  having(expr: ExprFn | ExprChain | Expr): ViewQueryBuilder;
  orderBy(items: OrderByItem[]): ViewQueryBuilder;
  limit(n: number): ViewQueryBuilder;
}

export interface CreateViewArgs {
  as: ((q: ViewQueryBuilder) => ViewQueryBuilder | SelectAst) | ViewQueryBuilder | SelectAst | { raw: string };
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
  drop(args?: DropViewArgs): ViewHandle;
  comment(text: string | null, args?: { schema?: string }): ViewHandle;
}

// ── `table()` entry + the fluent handle ──

/** The options bag `table(name, opts?)` accepts. Carries the default `{ schema }`
 *  every op the returned {@link TableHandle} records is stamped with (a per-op
 *  `schema` override wins; see {@link TableHandle}). An extensible object so a
 *  future per-table default can be added without a signature break. */
export interface TableOptions {
  /** The default schema qualifier propagated to every op the handle records.
   *  Names-are-strings (no live-schema binding); a per-op `schema` on an
   *  individual call overrides it. */
  schema?: string;
}

export type ExclusionTarget = string | ExprFn | ExprChain | Expr;
/** Column index element object form. Use `order: "desc"` to render `col DESC`;
 *  `order: "asc"` and omitted order serialize as the default ASC shape. */
export interface IndexColumnElementArg {
  column: string;
  order?: IndexSortOrder;
  opclass?: string;
  collation?: string;
  nulls?: "first" | "last";
}

export interface IndexExprElementArg {
  expr: IndexExprFn;
  order?: IndexSortOrder;
  opclass?: string;
  collation?: string;
  nulls?: "first" | "last";
}

export type IndexElementArg =
  | string
  | IndexColumnElementArg
  | IndexExprElementArg;

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
  where?: IndexExprFn | ExprChain | Expr;
  deferrable?: boolean;
  initiallyDeferred?: boolean;
}

export type ExclusionAddArgs = ExclusionConstraintArgs & {
  ifNotExists?: boolean;
  schema?: string;
};

export type TableStrictness = "strict" | "lenient" | "off";

export interface TableRuntimeOptions {
  softDelete?: boolean;
  versioning?: boolean;
  strictness?: TableStrictness;
}

/** The all-object `create({...})` payload. Table-level constraints/indexes
 *  are FIELDS (no `build` callback — "no exceptions"); each carries a required
 *  `name` (name-first).
 *
 *  Apply-level lowering (what reaches the live DDL):
 *  - `uniques`, `foreignKeys`, `indexes` LOWER to DDL on Postgres (a named UNIQUE
 *    + a single-`id` FOREIGN KEY + extra indexes appear in the live catalog).
 *  - `indexes` also lower on SQLite (plain btree); a table-level `uniques` /
 *    `foreignKeys` on SQLite is a HARD authoring error (the SQLite CREATE renders
 *    from the column descriptor — a table-level constraint is not threaded into
 *    the emitter; refused fail-closed rather than silently dropped).
 *  - `foreignKeys` can name one or more local columns and one or more referenced
 *    columns. Composite/non-`id` forms are PostgreSQL-only in the current engine.
 *  - `checks` lower from the closed `Expr` AST through the engine renderer.
 *    Partial-index `where` renders on PostgreSQL and SQLite; MySQL refuses it
 *    fail-closed because MySQL has no partial indexes.
 *  - `primaryKey` is the ordered table-level spelling for composite keys. A
 *    column's `.primaryKey()` is valid only as a single-column shorthand; repeated
 *    column facets and conflicting table/column declarations are rejected.
 *    Current confined/platform policy still decides later whether an authored PK
 *    is accepted, rejected, or replaced by the system shape.
 *
 *  None of the above is ever a silent no-op — an unsupported spec fails closed at
 *  lower time. */
export interface CreateTableArgs {
  columns: Record<string, ColumnDef>;
  /** Collection runtime metadata, carried into `schema.runtime.json`.
   *  `softDelete`, `versioning`, and `strictness` share the same named bag as
   *  `table(name).setOptions(...)`. */
  options?: TableRuntimeOptions;
  /** Table primary key intent: undefined leaves the policy default unresolved,
   *  null requests no PK, and a non-empty ordered column-name array records an
   *  explicit single-column or composite PK. */
  primaryKey?: string[] | null;
  uniques?: Array<{ name: string; columns: string[] }>;
  checks?: CheckDef[];
  foreignKeys?: Array<{
    name: string;
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
    deferrable?: boolean;
    initiallyDeferred?: boolean;
  }>;
  exclusions?: Array<{ name: string } & ExclusionConstraintArgs>;
  indexes?: Array<{
    name: string;
    on: IndexElementArg[];
    unique?: boolean;
    using?: IndexMethod;
    /** Partial-index predicate. Renders on PostgreSQL and SQLite; MySQL refuses
     *  it fail-closed because MySQL has no partial indexes. */
    where?: IndexExprFn;
    include?: readonly string[];
    with?: IndexStorageParamsArg;
    only?: boolean;
    /** PG 15+ `NULLS NOT DISTINCT` on a UNIQUE index. PG-vendor: fails closed at
     *  validate on SQLite/MySQL. */
    nullsNotDistinct?: boolean;
  }>;
  partitionBy?: PartitionByInput;
  ifNotExists?: boolean;
  /** Overrides the handle default schema. */
  schema?: string;
}

/** The `.column(name)` selector sub-handle. Each terminal records eagerly
 *  and returns the parent {@link TableHandle} (so chaining + var-reuse work). */
export interface ColumnRef {
  /** Add the column. Honors ALL modifiers on `type` — including `.unique()`
   *  (emits a follow-on unique constraint) and `.primaryKey()` (emits a follow-on
   *  pk). When BOTH are set, the redundant UNIQUE is suppressed (a PRIMARY KEY
   *  already implies uniqueness) — only the pk add is recorded. */
  add(args: { type: ColumnDef; ifNotExists?: boolean; schema?: string }): TableHandle;
  drop(args?: { ifExists?: boolean; schema?: string }): TableHandle;
  /** Named ⇒ no from/to swap. `type` is the column's type after rename. */
  rename(args: { to: string; type: ColumnDef; schema?: string }): TableHandle;
  setType(args: { to: ColumnDef; using?: ExprFn; schema?: string }): TableHandle;
  setNotNull(args?: { schema?: string }): TableHandle;
  dropNotNull(args?: { schema?: string }): TableHandle;
  setDefault(value: DefaultValue | DefaultExprFn | ExprChain | Expr, args?: { schema?: string }): TableHandle;
  dropDefault(args?: { schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
}

/** The `.foreignKey(name)` selector sub-handle. */
export interface ForeignKeyRef {
  add(args: {
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
    deferrable?: boolean;
    initiallyDeferred?: boolean;
    /** PostgreSQL-only online constraint adoption — add `NOT VALID` (skip the
     *  add-time scan), then `table(...).constraint(name).validate()` later. */
    notValid?: boolean;
    ifNotExists?: boolean;
    schema?: string;
  }): TableHandle;
}

/** The `.unique(name)` selector sub-handle. */
export interface UniqueRef {
  add(args: { columns: string[]; ifNotExists?: boolean; schema?: string }): TableHandle;
}

/** The `.check(name)` selector sub-handle. */
export interface CheckRef {
  add(args: {
    expr: CheckExprFn;
    /** PostgreSQL-only online constraint adoption — add `NOT VALID`, then
     *  `table(...).constraint(name).validate()` later. */
    notValid?: boolean;
    ifNotExists?: boolean;
    schema?: string;
  }): TableHandle;
}

/** The `.exclusion(name)` selector sub-handle. PostgreSQL renders native
 *  `EXCLUDE`; SQLite/MySQL fail closed. */
export interface ExclusionRef {
  add(args: ExclusionAddArgs): TableHandle;
}

/** The `.constraint(name)` selector sub-handle — kind-agnostic operations
 *  on an existing named constraint. */
export interface ConstraintRef {
  drop(args?: { ifExists?: boolean; schema?: string }): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
  /** PostgreSQL-only — validate a previously `NOT VALID` FK/CHECK against existing
   *  rows under a weaker lock (records a `validateConstraint` Op). */
  validate(args?: { ifExists?: boolean; schema?: string }): TableHandle;
}

/** The `.index(name)` selector sub-handle. */
export interface IndexAddArgs {
  on: IndexElementArg[];
  unique?: boolean;
  ifNotExists?: boolean;
  schema?: string;
  using?: IndexMethod;
  /** Partial-index predicate. Renders on PostgreSQL and SQLite; MySQL fails closed. */
  where?: IndexExprFn;
  include?: readonly string[];
  with?: IndexStorageParamsArg;
  only?: boolean;
  /** PG 15+ `NULLS NOT DISTINCT` on a UNIQUE index. PG-vendor: fails closed at
   *  validate on SQLite/MySQL. */
  nullsNotDistinct?: boolean;
}

export interface IndexDropArgs {
  ifExists?: boolean;
  schema?: string;
  concurrently?: boolean;
}

export interface IndexRef {
  add(args: IndexAddArgs): TableHandle;
  /** Drop the index. Uniqueness is derived by the engine from the live schema. */
  drop(args?: IndexDropArgs): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
}

/**
 * The recorder-bound handle `table(name, opts?)` returns. It is a REUSABLE value
 * carrying only `{ name, schemaDefault }`; every terminal records EAGERLY onto the
 * ambient recorder and returns the handle, so it is valid for unlimited chaining
 * + var-reuse. The
 * `{ schema }` from `table()` is the DEFAULT injected into every recorded op; a
 * per-op `schema` overrides it (by key presence — an absent/`undefined` per-op
 * schema keeps the table default).
 *
 * Direct methods exist for the table itself + its data; selector
 * sub-handles (`.column`/`.foreignKey`/`.unique`/`.check`/`.constraint`/`.index`)
 * exist for the table's named sub-objects. A selector that is
 * never terminated is a hard `SELECTOR_NOT_TERMINATED` build error at drain.
 */
export interface TableHandle {
  // The table itself (all-object terminals).
  create(args: CreateTableArgs): TableHandle;
  drop(args?: { ifExists?: boolean; cascade?: boolean; schema?: string }): TableHandle;
  /** Rename the whole table to `to` (a fast `ALTER TABLE … RENAME TO …` — NOT the
   *  online column expand-contract; `ifExists` guards the source table). Records a
   *  `renameTable` Op; the engine emits the inverse rename as the down-migration. */
  rename(args: { to: string; ifExists?: boolean; schema?: string }): TableHandle;
  setOptions(args: TableRuntimeOptions): TableHandle;
  comment(text: string | null, args?: { schema?: string }): TableHandle;
  partition(name: string): PartitionRef;

  // Selectors for named sub-objects
  column(name: string): ColumnRef;
  // One grammar, one spelling — the selector form is THE grammar for
  // named constraints. The `addForeignKey`/`addCheck` verb twins are DELETED;
  // `foreignKey(name).add(...)`/`check(name).add(...)` are the sole spelling
  // (and the sole public writers of the `addConstraint` fk/check payload).
  foreignKey(name: string): ForeignKeyRef;
  unique(name: string): UniqueRef;
  check(name: string): CheckRef;
  constraint(name: string): ConstraintRef;
  index(name: string): IndexRef;
  exclusion(name: string): ExclusionRef;
  setRls(args: { enabled?: boolean; forced?: boolean }): TableHandle;
  policy(name: string): PolicyRef;

  // Table data (direct named DML; no existence guard — DML is unguardable)
  insert<R extends Row = Row>(args: InsertArgs<R>): TableHandle;
  update(args: UpdateArgs): TableHandle;
  delete(args: DelArgs): TableHandle;
  backfill(args: BackfillArgs): TableHandle;

  // Cross-dialect core triggers.
  trigger(name: string): TriggerRef;
}

/** One determinism-lint finding. */
export interface DeterminismFinding {
  code: "NONDETERMINISTIC_OP_ARG";
  accessor: string;
  suggested_fix: string;
  reason: string;
}

/** The migration module shape: `export default { name?, up, down? }`. */
export interface Migration {
  name?: string;
  up(): void;
  down?(): void;
}
