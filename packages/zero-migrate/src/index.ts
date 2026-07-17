// `zero-migrate` — the no-raw-SQL, fully-structured, FLUENT-only op builder
// for portable bi-dialect (PG + SQLite) migrations (design
// `2026-06-25-op-dsl-fluent-redesign.md`).
//
// A migration is a `.ts` module that imports `{ table, t }`, and exports a single
// `default { up, down? }` object whose parameterless `up()`/`down()` author
// against the ambient per-migration recorder via `table()`. Names are plain
// strings (NOT live-schema-bound). Every expression is the fluent `(c) => Expr`
// builder — there is no ad-hoc raw *expression* escape and no `Raw` expr type
// (property A). The one deliberate escape hatch is the top-level `raw({ sql,
// reason })` DDL op (a trust-gated, reason-required whole-statement emitter for
// vendor DDL the structured surface cannot yet express) — exported below.
//
//   import { table, t } from "zero-migrate";
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
  domain,
  schema,
  extension,
  role,
  sequence,
  comment,
  check,
  lit,
  int64,
  decimal,
  byteValue,
  dialect,
  now,
  uuidV4,
  uuidV7,
  genRandomUuid,
  currentSetting,
  currentUser,
  interval,
  concatWs,
  countStar,
  minValue,
  maxValue,
  nextval,
  dropOwnedBy,
  grant,
  revoke,
  createFunction,
  dropFunction,
  raw,
  // the immutable fluent column-type lexicon
  t,
  // validated textual ID formats (storage + validation only)
  ids,
  // apply-engine ID generators evaluated once for every backfilled row
  perRow,
  // the shared `db` lexicon bridge: lift a live-schema
  // `t.*` field into a migration ColumnDef through the one shared ColType lexicon
  fromDb,
  // the determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";

export type {
  CreateFunctionArgs,
  DropFunctionArgs,
  DropOwnedByArgs,
  GrantArgs,
  PgRawArgs,
  RevokeArgs,
} from "./ops.js";

// The single-source db field → migration `ColType` reduction + its
// structured boundary error. The JS inverse of the engine's Rust
// `col_type_to_token`; the proof the migration DSL and the runtime schema share
// ONE type lexicon.
export { colTypeFromDbField, UnsupportedColTypeError } from "./db-lexicon.js";
export type { DbSchemaField, DbFieldType } from "./db-lexicon.js";

// The inlined db type-builder surface (`fromDb`/`colTypeFromDbField` input side).
// `dbType` is the `t.*` lexicon a live db schema field is authored with; the
// migrate bridge lifts it into a migration ColumnDef. Exposed here so a caller
// (and the bridge tests) can construct db fields without a separate db package.
export { t as dbType, TypeBuilder as DbTypeBuilder } from "./db-types.js";
export type { FieldDef, TypeName, EncryptedOptions } from "./db-types.js";

export type {
  // authoring types
  ColumnDef,
  TypeLexicon,
  IdFormats,
  PerRowGeneratorValue,
  PerRowGenerators,
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
  Int64Value,
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
  PrimaryKeyOperations,
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
  PolicyCreateArgs,
  PolicyDropArgs,
  PolicyRef,
  CreateTableArgs,
  TableRuntimeOptions,
  TableStrictness,
  OrderedColumns,
  ForeignKeyReference,
  TableForeignKey,
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
  AlterSequenceArgs,
  CreateDomainArgs,
  CreateSequenceArgs,
  DomainCheckFn,
  DomainHandle,
  DomainValueBuilder,
  DroppedExtensionHandle,
  DroppedRoleHandle,
  DroppedSchemaHandle,
  DropDomainArgs,
  DropSequenceArgs,
  ExtensionCreateArgs,
  ExtensionDropArgs,
  ExtensionHandle,
  RoleCreateArgs,
  RoleDropArgs,
  RoleHandle,
  RoleSetOptionsArgs,
  SchemaCreateArgs,
  SchemaDropArgs,
  SchemaHandle,
  SequenceHandle,
  SequenceOwnedBy,
  // op-arg shapes
  InsertArgs,
  UpdateArgs,
  DelArgs,
  BackfillArgs,
  BackfillSetValue,
  CursorStability,
  CreateViewArgs,
  DropViewArgs,
  ViewQueryBuilder,
  GroupByItem,
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
  ValueFormat,
  PerRowGenerator,
  TypeIdOptions,
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

export type {
  FuncArg,
  FuncArgMode,
  FuncLanguage,
  FuncVolatility,
  GrantTarget,
  PolicyCmd,
  Privilege,
} from "./generated/ir.js";

// The full generated dialect-neutral IR wire types (`Op`, `IrConstraint`,
// `MigrationIr`, …) — generated from the engine's `ir-envelope.schema.json`. Re-exported
// AS ERGONOMICS so an advanced caller can name the exact serde shape; the golden
// IR envelope corpus remains the source of truth.
export type * as ir from "./generated/ir.js";
