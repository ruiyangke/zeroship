// The dialect-neutral IR STRUCTURAL types (`MigrationIr`, `Op`, `Expr`,
// `ColType`, `IrConstraint`, …), HAND-AUTHORED as a faithful transcription of the
// engine's single-source-of-truth schema `crates/zeroship-migrate/op-ir.schema.json`.
//
// WHY HAND-AUTHORED (not generated): these defs form a self-recursive `oneOf` AST
// (`Expr` → `BinOp.lhs: Expr`; `ColType` → `encrypted.of: ColType`; `Op` carries
// `Expr`), which `json-schema-to-typescript` v15 cannot express — it inlines the
// `$ref` cycle and overflows the stack. So per PR3 ("manual types for any serde
// shape codegen cannot express"), the recursive structural types are authored
// here, while the closed STRING-ENUM tokens (`BinaryOp`, `SynthFn`, `CastTarget`,
// …) are GENERATED into `./enums.ts` and imported below.
//
// DRIFT GUARD: `tests/ir-types-drift.test.ts` pins every enum token, every `Op`
// variant tag, and every `Expr` node tag in THIS file against the schema, so the
// manual transcription cannot silently drift from the engine contract.
//
// These types are ERGONOMICS for an advanced caller; the golden `.ir.json` corpus
// + the `Checksum::of_ir` round-trip are the contract source of truth (§4.3/PR3).

import type {
  BinaryOp,
  CastTarget,
  CmpOp,
  IndexMethod,
  OnUnmet,
  OnlinePhase,
  ScalarFn,
  SynthDefaultFn,
  SynthFn,
  UnaryOp,
} from "./enums.js";

export type {
  BinaryOp,
  CastTarget,
  CmpOp,
  IndexMethod,
  OnUnmet,
  OnlinePhase,
  ScalarFn,
  SynthDefaultFn,
  SynthFn,
  UnaryOp,
};

/** A typed scalar (the §2.5 numeric domain): null / bool / safe-int / string /
 *  decimal-string / base64-bytes. */
export type IrScalar =
  | null
  | boolean
  | number
  | string
  | { decimal: string }
  | { bytes: string };

/** The dialect-NEUTRAL column-type lexicon (§3.2). Closed; camel-cased on the
 *  wire. `encrypted.of` is itself a `ColType` (the recursive arm). */
export type ColType =
  | "string"
  | "text"
  | "int"
  | "bigInt"
  | "float"
  | "bool"
  | "json"
  | "timestamp"
  | "uuid"
  | "bytea"
  | "geoPoint"
  | { ref: { references: string } }
  | { vector: { vector: number } }
  | { decimal: { precision: number; scale: number } }
  | { encrypted: { of: ColType } };

/** A column DEFAULT — a typed scalar literal OR a nullary synth scalar
 *  (`now`/`genRandomUuid`). Never raw SQL (property A). */
export type IrDefault =
  | { literal: { value: IrScalar } }
  | { fn: { fn: SynthDefaultFn } };

/** The CLOSED expression AST node (§3.3.1), internally tagged on `node`. */
export type Expr =
  | { node: "colRef"; name: string }
  | { node: "literal"; value: IrScalar }
  | { node: "binOp"; op: BinaryOp; lhs: Expr; rhs: Expr }
  | { node: "unaryOp"; op: UnaryOp; operand: Expr }
  | { node: "case"; branches: CaseBranch[]; else?: Expr | null }
  | { node: "fnCall"; fn: ScalarFn; args: Expr[] }
  | { node: "fnSynth"; fn: SynthFn; args: Expr[] }
  | { node: "cast"; operand: Expr; target: CastTarget };

/** One `(condition, result)` branch of an `Expr` `case`. */
export interface CaseBranch {
  condition: Expr;
  result: Expr;
}

/** A column definition inside `createTable` / `addColumn`. */
export interface IrColumn {
  name: string;
  type: ColType;
  nullable?: boolean | null;
  default?: IrDefault | null;
  unique?: boolean | null;
}

/** The kind of a table constraint (closed, internally tagged on `kind`). */
export type IrConstraintKind =
  | { kind: "pk"; columns: string[] }
  | { kind: "fk"; columns: string[]; referencesTable: string; referencesColumns: string[] }
  | { kind: "unique"; columns: string[] }
  | { kind: "check"; expr: Expr };

/** A named table constraint (the `kind` is a nested internally-tagged object). */
export interface IrConstraint {
  name?: string | null;
  kind: IrConstraintKind;
}

/** An index definition inside a `createTable` op. */
export interface IrIndex {
  name?: string | null;
  columns: string[];
  unique?: boolean | null;
  using?: IndexMethod | null;
  where?: Expr | null;
}

/** The optional `insert { onConflict }` upsert clause (PG-only). */
export interface IrOnConflict {
  columns: string[];
  doUpdate?: { [column: string]: IrScalar } | null;
}

/** A batched-backfill / batched-update knob. */
export interface IrBatch {
  cursorColumn: string;
  batchSize: number;
}

/** The CLOSED `op.*` operation enum (§2.3), internally tagged on `op`,
 *  camel-cased. NOTE the `del()` DSL function records the `"delete"` variant tag. */
export type Op =
  | { op: "createTable"; name: string; columns: IrColumn[]; constraints?: IrConstraint[]; indexes?: IrIndex[] }
  | { op: "dropTable"; table: string; ifExists?: boolean | null; cascade?: boolean | null }
  | { op: "addColumn"; table: string; column: string; type: ColType; nullable?: boolean | null; default?: IrDefault | null }
  | { op: "dropColumn"; table: string; column: string; ifExists?: boolean | null }
  | {
      op: "createIndex";
      table: string;
      columns: string[];
      name?: string | null;
      unique?: boolean | null;
      using?: IndexMethod | null;
      where?: Expr | null;
      concurrently?: boolean | null;
    }
  | { op: "dropIndex"; name: string; table?: string | null; unique?: boolean | null; ifExists?: boolean | null; concurrently?: boolean | null }
  | { op: "alterColumnType"; table: string; column: string; type: ColType; using?: Expr | null }
  | { op: "alterColumnNullability"; table: string; column: string; nullable: boolean }
  | { op: "renameColumn"; table: string; from: string; to: string; type: ColType }
  | { op: "addConstraint"; table: string; constraint: IrConstraint }
  | { op: "dropConstraint"; table: string; name: string }
  | { op: "insert"; table: string; columns: string[]; rows: IrScalar[][]; onConflict?: IrOnConflict | null }
  | { op: "update"; table: string; set: { [column: string]: Expr }; where?: Expr | null; batch?: IrBatch | null }
  | { op: "delete"; table: string; where: Expr; limit?: number | null }
  | { op: "backfill"; table: string; cursorColumn: string; batchSize: number; set: { [column: string]: Expr }; filter?: Expr | null; name: string };

/** All-`Option` overrides of the migration flags. */
export interface IrFlagsOverride {
  transactional?: boolean | null;
  destructive?: boolean | null;
  online?: boolean | null;
  requires_approval?: boolean | null;
  repeatable?: boolean | null;
  engine_goodie_ddl?: boolean | null;
  timeout_ms?: number | null;
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

/** The portable migration IR document (`.ir.json`, §2.1). */
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
