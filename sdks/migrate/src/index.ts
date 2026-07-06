// `@zeroship/migrate` — the no-raw-SQL, fully-structured, FLUENT-only op builder
// for portable bi-dialect (PG + SQLite) migrations (design
// `2026-06-25-op-dsl-fluent-redesign.md`).
//
// A migration is a `.ts` module that imports `{ table, t }`, and exports a single
// `default { up, down? }` object whose parameterless `up()`/`down()` author
// against the ambient per-migration recorder via `table()`. Names are plain
// strings (NOT live-schema-bound). Every expression is the fluent `(c) => Expr`
// builder; there is no raw escape and no `Raw` type (property A).
//
//   import { table, t } from "@zeroship/migrate";
//
//   export default {
//     up() {
//       table("users")
//         .column("first_name").add({ type: t.text() })
//         .backfill({ set: { first_name: c => c("name").splitPart(" ", 1) } });
//     },
//   };

export {
  // table DDL/DML entry — the reusable fluent TableHandle
  table,
  // cross-dialect view authoring entry — emits the closed SelectAst by default
  view,
  enumType,
  comment,
  check,
  lit,
  decimal,
  byteValue,
  dialect,
  now,
  genRandomUuid,
  currentSetting,
  currentUser,
  interval,
  concatWs,
  countStar,
  minValue,
  maxValue,
  nextval,
  // the immutable fluent column-type lexicon
  t,
  // the shared `@zeroship/db` lexicon bridge (PR5 goal A): lift a live-schema
  // `t.*` field into a migration ColumnDef through the one shared ColType lexicon
  fromDb,
  // the determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";

// The single-source `@zeroship/db` field → migration `ColType` reduction (PR5
// goal A) + its structured boundary error. The JS inverse of the engine's Rust
// `col_type_to_token`; the proof the migration DSL and the runtime schema share
// ONE type lexicon.
export { colTypeFromDbField, UnsupportedColTypeError } from "./db-lexicon.js";
export type { DbSchemaField, DbFieldType } from "./db-lexicon.js";

export type {
  // authoring types
  ColumnDef,
  TypeLexicon,
  ExprBuilder,
  ExprChain,
  ExprFn,
  ExtractField,
  PgExtractField,
  CheckBuilder,
  CheckDef,
  CheckExprFn,
  Duration,
  Scalar,
  ScalarValue,
  DecimalValue,
  BytesValue,
  CurrentSettingOptions,
  EmptyContainerDefault,
  JsonDefaultValue,
  JsonDefaultObject,
  NextvalDefault,
  NextvalOptions,
  SequenceRef,
  DefaultBuilder,
  DefaultExprFn,
  DefaultValue,
  Row,
  Migration,
  // the fluent handle + selector sub-handles
  TableHandle,
  TableOptions,
  ViewHandle,
  ViewOptions,
  ColumnRef,
  ForeignKeyRef,
  UniqueRef,
  CheckRef,
  ExclusionRef,
  ConstraintRef,
  IndexRef,
  IndexAddArgs,
  IndexDropArgs,
  CreateTableArgs,
  TableRuntimeOptions,
  TableStrictness,
  ForeignKeyReference,
  ExclusionTarget,
  ExclusionElementArg,
  ExclusionConstraintArgs,
  ExclusionAddArgs,
  IndexElement,
  IndexElementArg,
  CommentTarget,
  CommentTargetArg,
  EnumHandle,
  CreateEnumArgs,
  DropEnumArgs,
  // op-arg shapes
  InsertArgs,
  UpdateArgs,
  DelArgs,
  BackfillArgs,
  CreateViewArgs,
  DropViewArgs,
  ViewQueryBuilder,
  IndexMethod,
  IndexStorageParams,
  IndexStorageParamsArg,
  PartitionSpec,
  PartitionBounds,
  PartitionBoundValue,
  PartitionByInput,
  PartitionBoundArgs,
  PartitionBoundInput,
  PartitionBoundSentinel,
  PartitionRef,
  TriggerRef,
  TriggerCreateArgs,
  TriggerDropArgs,
  CreatePartitionOptions,
  DropPartitionArgs,
  DetachPartitionArgs,
  AttachPartitionArgs,
  ExclusionMethod,
  ExclusionOperator,
  RefAction,
  DeterminismFinding,
  // sensitive-data column facets (#173/#174/#178)
  MaskKind,
  Classification,
  VectorMetric,
  IdOptions,
  TextOptions,
  NumericOptions,
  CharOptions,
  VectorOptions,
  MaskOptions,
  // re-exported generated IR wire types (ergonomics; goldens are the contract)
  ColType,
  Expr,
  IrBatch,
  IrJsonValue,
  IrScalar,
  ViewQuery,
  SelectAst,
  TableRef,
  SelectItem,
  Join,
  JoinKind,
  OrderItem,
  OrderDir,
} from "./types.js";

// The full generated dialect-neutral IR wire types (`Op`, `IrConstraint`,
// `MigrationIr`, …) — generated from the engine's `op-ir.schema.json`. Re-exported
// AS ERGONOMICS so an advanced caller can name the exact serde shape; the golden
// `.ir.json` corpus remains the source of truth.
export type * as ir from "./generated/ir.js";
