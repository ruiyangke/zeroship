// `@zeroship/migrate` — the no-raw-SQL, fully-structured, named-ESM-import op
// builder for portable bi-dialect (PG + SQLite) migrations (§3.2/§3.3.1).
//
// A migration is a `.ts` module that imports the op-functions it uses and exports
// a single `default { up, down? }` object whose parameterless `up()`/`down()`
// record into the ambient per-migration recorder (§3.1). Names are plain strings
// (NOT live-schema-bound — §3.3). Every expression is the fluent `(c) => Expr`
// builder; there is no raw escape and no `Raw` type (property A).
//
//   import { createTable, addColumn, backfill, t } from "@zeroship/migrate";
//
//   export default {
//     up() {
//       addColumn("users", "first_name", t.text());
//       backfill("users", { set: { first_name: c => c.fn.splitPart(c("name"), " ", 1) } });
//     },
//   };

export {
  // DDL: tables
  createTable,
  dropTable,
  // DDL: columns
  addColumn,
  dropColumn,
  renameColumn,
  alterColumn,
  // DDL: constraints / indexes
  addForeignKey,
  addUnique,
  addCheck,
  dropConstraint,
  createIndex,
  dropIndex,
  // DML
  insert,
  update,
  del, // `del`, not `delete` — `delete` is a JS reserved word
  backfill,
  // SQLite-safe rebuild
  batchAlterTable,
  // the fluent column-type lexicon
  t,
  // the §4.3 determinism lint (best-effort source scan)
  lintDeterminism,
} from "./ops.js";

export type {
  // authoring types
  ColumnDef,
  ColumnOpts,
  TypeLexicon,
  ExprBuilder,
  ExprChain,
  ExprFn,
  FnNamespace,
  ScalarValue,
  Row,
  Migration,
  // op-arg shapes
  ForeignKeySpec,
  UniqueSpec,
  CheckSpec,
  DropConstraintSpec,
  CreateIndexSpec,
  AlterColumnChange,
  InsertArgs,
  UpdateArgs,
  DelArgs,
  BackfillArgs,
  TableBuilder,
  BatchAlterBuilder,
  IndexMethod,
  FkAction,
  DeterminismFinding,
  // re-exported generated IR wire types (ergonomics; goldens are the contract)
  ColType,
  Expr,
  IrBatch,
  IrScalar,
} from "./types.js";

// The full generated dialect-neutral IR wire types (`Op`, `IrConstraint`,
// `MigrationIr`, …) — generated from the engine's `op-ir.schema.json`. Re-exported
// AS ERGONOMICS so an advanced caller can name the exact serde shape; the golden
// `.ir.json` corpus remains the source of truth (§4.3 / PR3).
export type * as ir from "./generated/ir.js";
