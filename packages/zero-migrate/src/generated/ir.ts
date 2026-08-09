// The dialect-neutral IR STRUCTURAL types (`MigrationIr`, `Op`, `Expr`,
// `ColType`, `IrConstraint`, …), HAND-AUTHORED as a faithful transcription of the
// engine's single-source-of-truth schema `crates/zero-migrate/ir-envelope.schema.json`.
//
// WHY HAND-AUTHORED (not generated): these defs form a self-recursive `oneOf` AST
// (`Expr` → `BinOp.lhs: Expr`; `ColType` → `encrypted.of: ColType`; `Op` carries
// `Expr`), which `json-schema-to-typescript` v15 cannot express — it inlines the
// `$ref` cycle and overflows the stack. So — manual types for any serde
// shape codegen cannot express — the recursive structural types are authored
// here, while the closed STRING-ENUM tokens (`BinaryOp`, `SynthFn`, `CastTarget`,
// …) are GENERATED into `./enums.ts` and imported below.
//
// DRIFT GUARD: `tests/ir-types-drift.test.ts` pins every enum token, every `Op`
// variant tag, and every `Expr` node tag in THIS file against the schema, so the
// manual transcription cannot silently drift from the engine contract.
//
// These types are ERGONOMICS for an advanced caller; the golden IR envelope corpus
// + the `Checksum::of_ir` round-trip are the contract source of truth.

import type {
  AggFunc,
  BinaryOp,
  CastTarget,
  CmpOp,
  EmptyContainerKind,
  ExistenceGuard,
  ExclusionMethod,
  ExclusionOperator,
  ExtractField,
  ForEach,
  FuncArgMode,
  FuncLanguage,
  FuncVolatility,
  IndexMethod,
  IndexSortOrder,
  JoinKind,
  OnUnmet,
  OnlinePhase,
  OrderDir,
  PolicyCmd,
  PgExtractField,
  Privilege,
  RaiseLevel,
  RefAction,
  ScalarFn,
  SynthFn,
  TableStrictness,
  TriggerEvent,
  TriggerTiming,
  UnaryOp,
} from "./enums.js";

export type {
  AggFunc,
  BinaryOp,
  CastTarget,
  CmpOp,
  EmptyContainerKind,
  ExistenceGuard,
  ExclusionMethod,
  ExclusionOperator,
  ExtractField,
  ForEach,
  FuncArgMode,
  FuncLanguage,
  FuncVolatility,
  IndexMethod,
  IndexSortOrder,
  JoinKind,
  OnUnmet,
  OnlinePhase,
  OrderDir,
  PolicyCmd,
  PgExtractField,
  Privilege,
  RaiseLevel,
  RefAction,
  ScalarFn,
  SynthFn,
  TableStrictness,
  TriggerEvent,
  TriggerTiming,
  UnaryOp,
};

/** Signed integer constrained by the engine schema to the JS safe-integer range. */
export type SafeI64 = number;

/** Unsigned integer constrained by the engine schema to the JS safe-integer range. */
export type SafeU64 = number;

/** A typed scalar (the numeric domain): null / bool / safe-int / string /
 *  exact signed-int64 string / decimal-string / base64-bytes. */
export type IrScalar =
  | null
  | boolean
  | number
  | string
  | { int64: string }
  | { decimal: string }
  | { bytes: string };

/** The dialect-NEUTRAL column-type lexicon. Closed; camel-cased on the
 *  wire. `encrypted.of` is itself a `ColType` (the recursive arm). */
export type ColType =
  | { string: { length: number } }
  | "text"
  | "int"
  | "smallInt"
  | "bigInt"
  | "double"
  | "real"
  | "boolean"
  | "json"
  | "timestamp"
  | "date"
  | "uuid"
  | "inet"
  | "textArray"
  | "bytes"
  | "geoPoint"
  | { char: { length: number } }
  // The WEAKER of the two constructs in this file spelled `references`: a column
  // TYPE naming a table only. The other is the column FACET `IrColumn.references`
  // below, which names a table AND column and carries onDelete/onUpdate. Emitting
  // this one where the facet was meant silently drops the target column.
  | { ref: { references: string } }
  | { vector: { vector: number } }
  | { decimal: { precision: number; scale: number } }
  | { enum: { name: string; schema?: string } }
  | { domain: { name: string; schema?: string } }
  | { encrypted: { of: ColType } };

/** Canonical, validated value formats carried independently from physical
 * column storage. The variants use Serde's externally tagged encoding. */
export type ValueFormat = { typeId: { prefix: string } } | "ulid";

/** The CLOSED pgvector distance-metric lexicon — drives the ivfflat/hnsw
 *  operator class. Camel-cased on the wire; faithful transcription of the schema
 *  `VectorMetric` `oneOf` const set. A DECLARED-ONLY hint introspection cannot
 *  recover, so it is carried on the column. */
export type VectorMetric = "cosine" | "l2" | "innerProduct";

/** The CLOSED column-masking transform lexicon (`.mask({ kind })`, #174) — faithful
 *  transcription of the schema `IrMaskKind` `oneOf` const set. The two date forms
 *  are KEBAB (`date-year`/`date-decade`); the rest are single camelCase words. */
export type MaskKind =
  | "full"
  | "last4"
  | "first4"
  | "email"
  | "name"
  | "date-year"
  | "date-decade"
  | "none";

/** The CLOSED sensitivity-classification lexicon (`.mask({ classification })`,
 *  #174) — faithful transcription of the schema `IrClassification` `oneOf` const
 *  set. */
export type Classification = "public" | "pii" | "spi" | "phi" | "pci" | "internal";

/** A standalone column-masking facet (`.mask({ kind, classification })`, #174). */
export interface IrMask {
  kind: MaskKind;
  classification: Classification;
}

/** A closed sequence reference for `nextval(...)` defaults. */
export interface SequenceRef {
  name: string;
  schema?: string | null;
}

/** A canonical JSON value for non-empty json defaults. Numbers are integers only
 *  and must satisfy |v| < 2^53. */
export type IrJsonValue =
  | null
  | boolean
  | SafeI64
  | string
  | IrJsonValue[]
  | { [key: string]: IrJsonValue };

/** A column DEFAULT — a typed scalar literal, a closed expression AST, an empty
 *  container default, a non-empty JSON value default, or a PostgreSQL sequence
 *  `nextval` reference. Never raw SQL (property A). */
export type IrDefault =
  | { literal: { value: IrScalar } }
  | { expr: Expr }
  | { container: EmptyContainerKind }
  | { json: IrJsonValue }
  | { nextval: SequenceRef };

/** The CLOSED expression AST node, internally tagged on `node`. */
export type Expr =
  | { node: "colRef"; name: string; table?: string | null }
  | { node: "literal"; value: IrScalar }
  | { node: "binOp"; op: BinaryOp; lhs: Expr; rhs: Expr }
  | { node: "unaryOp"; op: UnaryOp; operand: Expr }
  | { node: "case"; branches: CaseBranch[]; else?: Expr | null }
  | { node: "fnCall"; fn: ScalarFn; args: Expr[] }
  | { node: "fnSynth"; fn: SynthFn; args: Expr[] }
  | { node: "uuidV4" }
  | { node: "uuidV7" }
  | { node: "cast"; operand: Expr; target: CastTarget }
  | { node: "between"; operand: Expr; low: Expr; high: Expr }
  | { node: "like"; operand: Expr; pattern: Expr }
  | { node: "distinctFrom"; left: Expr; right: Expr }
  | { node: "agg"; func: AggFunc; arg?: Expr | null; delimiter?: Expr | null; distinct?: boolean }
  | { node: "inList"; expr: Expr; elems: IrScalar[]; negated: boolean }
  | { node: "pgRegexMatch"; expr: Expr; pattern: string }
  | { node: "pgColumnSize"; expr: Expr }
  | { node: "extract"; field: ExtractField; from: Expr }
  | { node: "pgExtract"; field: PgExtractField; from: Expr }
  | { node: "pgInterval"; duration: Duration }
  // The one Layer-2 portability escape: a per-dialect value divergence.
  // Legs serialize in canonical order (default, pg, sqlite, mysql); a `None` leg
  // is skipped on the wire. Scope math is validated per-target by the engine.
  | { node: "dialect"; default?: Expr | null; pg?: Expr | null; sqlite?: Expr | null; mysql?: Expr | null };

/** A DML cell in an insert row, ordinary update, trigger update, or
 *  `onConflict.doUpdate`: either a typed scalar literal or a closed expression
 *  AST such as `fnSynth(now)`. */
export type IrValue = IrScalar | Expr;

/** An apply-engine generator evaluated independently for every affected row of
 *  a migration backfill. Unit variants use their canonical camelCase strings;
 *  TypeID carries its exact stored prefix. */
export type PerRowGenerator =
  | "uuidV4"
  | "uuidV7"
  | { typeId: { prefix: string } }
  | "ulid";

/** A backfill assignment is either an ordinary DML value or an explicitly
 *  wrapped apply-engine generator. The wrapper keeps per-row authority distinct
 *  from database SQL expressions such as `{ node: "uuidV4" }`. */
export type BackfillSetValue = IrValue | { perRow: PerRowGenerator };

/** One `(when, then)` branch of an `Expr` `case`. */
export interface CaseBranch {
  when: Expr;
  then: Expr;
}

export interface Duration {
  years?: number;
  months?: number;
  days?: number;
  hours?: number;
  minutes?: number;
  seconds?: number;
}

/** A generated/computed column facet. `expr` is the closed expression AST; `stored`
 *  is `true` for STORED and `false` for SQLite VIRTUAL. */
export interface GeneratedCol {
  expr: Expr;
  stored: boolean;
}

/** A SQL identity column facet. `always:true` means GENERATED ALWAYS; `false`
 *  means GENERATED BY DEFAULT. */
export interface IdentityCol {
  always: boolean;
}

/** Target and referential actions for a typed single-column reference. */
export interface ColumnReference {
  table: string;
  column: string;
  onDelete?: RefAction | null;
  onUpdate?: RefAction | null;
  name?: string | null;
}

/** A column definition inside `createTable`. Lifecycle operations use their own
 *  explicit fields and cannot carry a reference facet. */
export interface IrColumn {
  name: string;
  type: ColType;
  nullable?: boolean | null;
  default?: IrDefault | null;
  unique?: boolean | null;
  /** Canonical value-format semantics. `ids.typeId()` and `ids.ulid()` store
   *  text while this facet carries the exact persisted format. Default-absent. */
  valueFormat?: ValueFormat | null;
  /** Typed single-column foreign-key reference. The local `type` remains
   *  explicit and is never selected from the referenced target or catalog.
   *
   *  The STRONGER of the two constructs spelled `references`: a column facet
   *  carrying a target table AND column plus onDelete/onUpdate. The other is the
   *  `ColType` variant `{ ref: { references } }`, which names a table only.
   *  A transcription that stops at whichever it meets first loses the difference. */
  references?: ColumnReference | null;
  /** A legacy internal `<prefix>_<22 base62 UUIDv7>` platform-ID prefix, retained
   *  for old internal descriptors. It is not TypeID or public authoring.
   *  Camel-cased on the wire and default-absent. */
  idPrefix?: string | null;
  /** The `t.vector(n, { metric })` distance metric (closed
   *  {@link VectorMetric}). Default-absent. */
  vectorMetric?: VectorMetric | null;
  /** Case-sensitivity facet for text columns. Only `false` is meaningful;
   *  omitted/`true` is the byte-identical default. */
  caseSensitive?: boolean | null;
  /** **#174** — a STANDALONE column mask. Default-absent. */
  mask?: IrMask | null;
  /** Generated/computed column facet. Default-absent. */
  generated?: GeneratedCol | null;
  /** SQL identity column facet. Default-absent. */
  identity?: IdentityCol | null;
}

/** The kind of a table constraint (closed, internally tagged on `kind`). */
export type IrConstraintKind =
  | { kind: "fk"; columns: string[]; referencesTable: string; referencesColumns: string[]; onDelete?: RefAction | null; onUpdate?: RefAction | null; notValid?: boolean | null }
  | { kind: "unique"; columns: string[] }
  | { kind: "check"; expr: Expr; notValid?: boolean | null }
  | {
      kind: "exclusion";
      usingMethod?: ExclusionMethod;
      elements: ExclusionElement[];
      wherePredicate?: Expr | null;
      deferrable?: boolean | null;
      initiallyDeferred?: boolean | null;
    };

/** Exclusion target: a column name or a closed expression AST. */
export type ColumnOrExpr =
  | { kind: "column"; name: string }
  | { kind: "expr"; expr: Expr };

/** Index target: a column name or a closed expression AST. The `opclass` and
 *  `collation` per-column facets are PG-vendor (fail-closed off PostgreSQL). */
export type IndexElement =
  | {
      kind: "column";
      name: string;
      order?: IndexSortOrder | null;
      opclass?: string | null;
      collation?: string | null;
    }
  | { kind: "expr"; expr: Expr };

/** One `(target WITH operator)` element in an exclusion constraint. */
export interface ExclusionElement {
  target: ColumnOrExpr;
  operator: ExclusionOperator;
}

/** COMMENT ON target. Function comments are name-only in this IR; overloaded
 *  function signatures are intentionally not modeled. */
export type CommentTarget =
  | { kind: "table"; schema?: string | null; name: string }
  | { kind: "column"; schema?: string | null; table: string; name: string }
  | { kind: "index"; schema?: string | null; name: string }
  | { kind: "constraint"; schema?: string | null; table: string; name: string }
  | { kind: "view"; schema?: string | null; name: string }
  | { kind: "type"; schema?: string | null; name: string }
  | { kind: "sequence"; schema?: string | null; name: string }
  | { kind: "function"; schema?: string | null; name: string };

/** A named table constraint (the `kind` is a nested internally-tagged object). */
export interface IrConstraint {
  name?: string | null;
  kind: IrConstraintKind;
}

/** Optional sequence ownership target. */
export interface SequenceOwnedBy {
  table: string;
  column: string;
}

/** An index definition inside a `createTable` op. */
export interface IndexStorageParams {
  pagesPerRange?: number | null;
  fillfactor?: number | null;
}

export interface IrIndex {
  name?: string | null;
  columns: IndexElement[];
  unique?: boolean | null;
  using?: IndexMethod | null;
  where?: Expr | null;
  include?: string[];
  with?: IndexStorageParams | null;
  only?: boolean | null;
  /** PG 15+ `NULLS NOT DISTINCT` on a UNIQUE index. PG-vendor. */
  nullsNotDistinct?: boolean | null;
}

/** Partitioning strategy for a partitioned table parent. */
export type PartitionSpec =
  | { kind: "range"; columns: string[]; collapse?: boolean }
  | { kind: "list"; columns: string[]; collapse?: boolean }
  | { kind: "hash"; columns: string[]; collapse?: boolean };

/** Closed partition-bound literal. Never raw SQL. */
export type PartitionBoundValue =
  | { kind: "string"; value: string }
  | { kind: "int"; value: SafeI64 }
  | { kind: "minValue" }
  | { kind: "maxValue" };

/** Partition bounds for `CREATE TABLE child PARTITION OF parent`. */
export type PartitionBounds =
  | { kind: "range"; from: PartitionBoundValue[]; to: PartitionBoundValue[] }
  | { kind: "list"; values: PartitionBoundValue[] }
  | { kind: "hash"; modulus: number; remainder: number }
  | { kind: "default" };

/** Complete collection-level runtime options stamped on `createTable`. */
export interface TableRuntimeOptions {
  /** `schema(...).softDelete()`. */
  softDelete: boolean;
  /** `schema(...).withVersioning()`. */
  versioning: boolean;
  /** `schema(...).strictness(...)`; default matches `db`. */
  strictness?: TableStrictness;
}

/** Patch form for `setTableOptions`: absent fields mean "leave unchanged". */
export interface TableRuntimeOptionsPatch {
  /** Toggle soft-delete behaviour. */
  softDelete?: boolean | null;
  /** Toggle optimistic versioning behaviour. */
  versioning?: boolean | null;
  /** Change deploy-time validation strictness. */
  strictness?: TableStrictness | null;
}

/** Structured insert conflict handling. PostgreSQL and SQLite support both
 * forms. MySQL supports a non-empty doUpdate when its target can be guarded. */
export interface IrOnConflict {
  columns: string[];
  doUpdate?: { [column: string]: IrValue } | null;
}

/** The invariant that keeps a resumable backfill's cursor tuple immutable. */
export type CursorStability =
  | { mode: "guardUpdates" }
  | { mode: "externalInvariant"; name: string };

/** A batched-backfill knob. */
export interface IrBatch {
  cursorColumns: [string, ...string[]];
  cursorStability: CursorStability;
  batchSize: number;
}

/** The closed trigger action: either call an operator-provided function
 *  (PG render path) or carry a structured trigger body (SQLite render path). */
export type TriggerAction =
  | { kind: "executeFunction"; name: string }
  | { kind: "body"; statements: TriggerStmt[] };

/** One structured trigger body statement. Reuses the DML payload
 *  shapes where possible and adds the closed `Raise` node. */
export type TriggerStmt =
  | { stmt: "insert"; table: string; columns: string[]; rows: IrValue[][]; schema?: string | null }
  | { stmt: "update"; table: string; set: { [column: string]: IrValue }; where?: Expr | null; schema?: string | null }
  | { stmt: "delete"; table: string; where: Expr; limit?: number | null; schema?: string | null }
  | { stmt: "select"; expr: Expr }
  | { stmt: "raise"; level: RaiseLevel; message: string; errcode?: string | null };

/** A view body is either the closed, portable SelectAst subset or an
 *  operator-gated raw SELECT body. */
export type ViewQuery =
  | { kind: "structured"; select: SelectAst }
  | { kind: "raw"; sql: string };

/** The closed SELECT subset rendered by the engine for structured views. */
export interface SelectAst {
  from: TableRef;
  /** Empty means `*`. */
  projection: SelectItem[];
  joins?: Join[];
  where?: Expr | null;
  groupBy?: Expr[];
  having?: Expr | null;
  orderBy?: OrderItem[] | null;
  limit?: number | null;
}

export interface TableRef {
  name: string;
  schema?: string | null;
  alias?: string | null;
}

export type SelectItem =
  | { kind: "colRef"; table?: string | null; name: string; alias?: string | null }
  | { kind: "expr"; expr: Expr; alias?: string | null };

export interface Join {
  kind: JoinKind;
  table: TableRef;
  on: Expr;
}

export type OrderItem =
  | { kind: "colRef"; table?: string | null; name: string; dir?: OrderDir | null }
  | { kind: "expr"; expr: Expr; dir?: OrderDir | null };

/** **VENDOR** — one CREATE FUNCTION argument. */
export interface FuncArg {
  name?: string | null;
  type: string;
  mode?: FuncArgMode | null;
}

/** **VENDOR** — the closed GRANT/REVOKE target. */
export type GrantTarget =
  | { kind: "table"; names: string[]; schema?: string | null }
  | { kind: "schema"; names: string[] }
  | { kind: "sequence"; in: string }
  | { kind: "database"; names: string[] };

/** The explicit final primary-key constraint mutation. All columns, values,
 * candidate uniqueness, and foreign keys must already be staged. */
export type AlterPrimaryKeyAction =
  | { kind: "add"; columns: [string, ...string[]] }
  | {
      kind: "replace";
      expectedColumns: [string, ...string[]];
      columns: [string, ...string[]];
      dropIdentityFrom?: [string, ...string[]] | null;
    }
  | {
      kind: "drop";
      expectedColumns: [string, ...string[]];
      dropIdentityFrom?: [string, ...string[]] | null;
    };

/** The CLOSED `op.*` operation enum, internally tagged on `op`,
 *  camel-cased. NOTE the `del()` DSL function records the `"delete"` variant tag.
 *
 *  Every TABLE-TARGETING variant carries an optional `schema?` (the
 *  schema-qualifier — honored under Trusted/Platform, pinned/refused under
 *  Confined), and every GUARDABLE DDL variant additionally carries an optional
 *  `existenceGuard?`. The DML ops (`insert`/`update`/`delete`/`backfill`) carry
 *  `schema?` but NO `existenceGuard?` (DML has no existence semantics). The
 *  removed native `ifExists?: boolean` on `dropTable`/`dropColumn`/`dropIndex`/
 *  `dropView` is GONE (the intentional wire break) — the guard is now the uniform
 *  `existenceGuard?` token. */
export type Op =
  | { op: "createTable"; name: string; columns: IrColumn[]; primaryKey: string[] | null; constraints?: IrConstraint[]; indexes?: IrIndex[]; partitionBy?: PartitionSpec | null; runtimeOptions?: TableRuntimeOptions | null; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "createPartition"; name: string; of: string; bounds: PartitionBounds; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "attachPartition"; parent: string; name: string; bound: PartitionBounds; schema?: string | null }
  | { op: "detachPartition"; parent: string; name: string; schema?: string | null; concurrently?: boolean | null }
  | { op: "dropPartition"; parent: string; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null; cascade?: boolean | null }
  | { op: "dropTable"; table: string; cascade?: boolean | null; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "renameTable"; table: string; to: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "addColumn"; table: string; column: string; type: ColType; nullable?: boolean | null; default?: IrDefault | null; valueFormat?: ValueFormat | null; vectorMetric?: VectorMetric | null; caseSensitive?: boolean | null; mask?: IrMask | null; generated?: GeneratedCol | null; identity?: IdentityCol | null; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "dropColumn"; table: string; column: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | {
      op: "createIndex";
      table: string;
      columns: IndexElement[];
      name?: string | null;
      unique?: boolean | null;
      using?: IndexMethod | null;
      where?: Expr | null;
      concurrently?: boolean | null;
      include?: string[];
      with?: IndexStorageParams | null;
      only?: boolean | null;
      nullsNotDistinct?: boolean | null;
      schema?: string | null;
      existenceGuard?: ExistenceGuard | null;
    }
  | { op: "dropIndex"; name: string; table?: string | null; unique?: boolean | null; concurrently?: boolean | null; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "setColumnType"; table: string; column: string; toType: ColType; using?: Expr | null; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "setColumnNotNull"; table: string; column: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "dropColumnNotNull"; table: string; column: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "setColumnDefault"; table: string; column: string; value: IrDefault; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "dropColumnDefault"; table: string; column: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "renameColumn"; table: string; from: string; to: string; type: ColType; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "alterPrimaryKey"; table: string; action: AlterPrimaryKeyAction; schema?: string | null }
  | { op: "synchronizeIdentity"; table: string; column: string; writesQuiesced: string; schema?: string | null }
  | { op: "setTableOptions"; table: string; options: TableRuntimeOptionsPatch; schema?: string | null }
  | { op: "addConstraint"; table: string; constraint: IrConstraint; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "dropConstraint"; table: string; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "validateConstraint"; table: string; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "insert"; table: string; columns: string[]; rows: IrValue[][]; onConflict?: IrOnConflict | null; schema?: string | null }
  | { op: "update"; table: string; set: { [column: string]: IrValue }; where?: Expr | null; schema?: string | null }
  | { op: "delete"; table: string; where: Expr; limit?: number | null; schema?: string | null }
  | { op: "backfill"; table: string; cursorColumns: [string, ...string[]]; cursorStability: CursorStability; batchSize: number; set: { [column: string]: BackfillSetValue }; filter?: Expr | null; name: string; schema?: string | null }
  | { op: "dialectal"; default?: Op[] | null; pg?: Op[] | null; sqlite?: Op[] | null; mysql?: Op[] | null }
  | { op: "createView"; name: string; schema?: string | null; columns?: string[] | null; query: ViewQuery; replace?: boolean | null; materialized?: boolean | null }
  | { op: "dropView"; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null; materialized?: boolean | null }
  | { op: "createEnum"; name: string; schema?: string | null; values: string[] }
  | { op: "dropEnum"; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "createDomain"; name: string; schema?: string | null; as: ColType; check?: Expr | null; default?: IrDefault | null; notNull?: boolean | null }
  | { op: "dropDomain"; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | {
      op: "createSequence";
      name: string;
      schema?: string | null;
      as?: ColType | null;
      increment?: SafeI64 | null;
      start?: SafeI64 | null;
      minValue?: SafeI64 | null;
      maxValue?: SafeI64 | null;
      cache?: SafeU64 | null;
      cycle?: boolean | null;
      ownedBy?: SequenceOwnedBy | null;
    }
  | {
      op: "alterSequence";
      name: string;
      schema?: string | null;
      increment?: SafeI64 | null;
      restart?: SafeI64 | null;
      minValue?: SafeI64 | null;
      maxValue?: SafeI64 | null;
      cache?: SafeU64 | null;
      cycle?: boolean | null;
      ownedBy?: SequenceOwnedBy | null;
    }
  | { op: "dropSequence"; name: string; schema?: string | null; existenceGuard?: ExistenceGuard | null }
  | { op: "comment"; target: CommentTarget; comment?: string | null }
  | {
      op: "createTrigger";
      name: string;
      table: string;
      schema?: string | null;
      timing: TriggerTiming;
      events: TriggerEvent[];
      forEach: ForEach;
      action: TriggerAction;
      when?: Expr | null;
    }
  | { op: "dropTrigger"; name: string; table: string; schema?: string | null; ifExists?: boolean | null }
  | { op: "createSchema"; name: string; ifNotExists?: boolean | null; authorization?: string | null }
  | { op: "dropSchema"; name: string; ifExists?: boolean | null; cascade?: boolean | null }
  | { op: "createExtension"; name: string; ifNotExists?: boolean | null; schema?: string | null }
  | { op: "dropExtension"; name: string; ifExists?: boolean | null }
  | {
      op: "createRole";
      name: string;
      login?: boolean | null;
      password?: string | null;
      bypassRls?: boolean | null;
      createRole?: boolean | null;
      createDb?: boolean | null;
      superuser?: boolean | null;
      inRole?: string[] | null;
      setSearchPath?: string[] | null;
      ifNotExists?: boolean | null;
    }
  | { op: "alterRole"; name: string; setSearchPath?: string[] | null; resetSearchPath?: boolean | null }
  | { op: "dropRole"; name: string; ifExists?: boolean | null }
  | { op: "dropOwnedBy"; roles: string[] }
  | { op: "grant"; privileges: Privilege[]; on: GrantTarget; to: string[]; withGrantOption?: boolean | null }
  | { op: "revoke"; privileges: Privilege[]; on: GrantTarget; from: string[] }
  | { op: "setRls"; table: string; schema?: string | null; enabled?: boolean | null; forced?: boolean | null }
  | {
      op: "createPolicy";
      name: string;
      table: string;
      schema?: string | null;
      forCmd: PolicyCmd;
      to?: string[] | null;
      using: Expr;
      withCheck?: Expr | null;
    }
  | { op: "dropPolicy"; name: string; table: string; schema?: string | null; ifExists?: boolean | null }
  | {
      op: "createFunction";
      name: string;
      schema?: string | null;
      args?: FuncArg[] | null;
      returns: string;
      language: FuncLanguage;
      replace?: boolean | null;
      volatility?: FuncVolatility | null;
      body: string;
    }
  | { op: "dropFunction"; name: string; schema?: string | null; argTypes?: string[] | null; ifExists?: boolean | null }
  | { op: "pgRaw"; sql: string; reason: string };

/** All-`Option` overrides of the migration flags. */
export interface IrFlagsOverride {
  transactional?: boolean | null;
  destructive?: boolean | null;
  online?: boolean | null;
  requires_approval?: boolean | null;
  repeatable?: boolean | null;
  engine_goodie_ddl?: boolean | null;
  timeout_ms?: number | null;
  /** Per-deploy maintenance-window lock-acquisition budget (JS-safe-integer
   *  bounded); distinct from `timeout_ms` (the statement budget). */
  lock_timeout_ms?: number | null;
  phase?: OnlinePhase | null;
}

/** A single precondition assertion evaluated against the live DB. */
export type Precondition =
  | { TableExists: { table: string } }
  | { TableNotExists: { table: string } }
  | { ColumnExists: { table: string; column: string } }
  | { ColumnNotExists: { table: string; column: string } }
  | { RowCount: { table: string; op: CmpOp; value: number } }
  | { SqlBoolean: { sql: string } };

/** One precondition + its unmet policy. */
export interface PreconditionCheck {
  check: Precondition;
  on_unmet?: OnUnmet;
}

/** The portable migration IR document (IR envelope). */
export interface MigrationIr {
  ir_version: number;
  name: string;
  owner_app?: string;
  ops: Op[];
  flags?: IrFlagsOverride;
  depends_on?: string[];
  supersedes?: string[];
  preconditions?: PreconditionCheck[];
  checksum?: string | null;
}
