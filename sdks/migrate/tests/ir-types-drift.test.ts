// Drift guard for the hand-authored IR structural types (`src/generated/ir.ts`)
// + the generated enum tokens (`src/generated/enums.ts`): every `Op` variant tag,
// every `Expr` node tag, the `ColType` token set, and every closed string-enum
// token in the TS types is pinned against the engine's single-source-of-truth
// schema `crates/zeroship-migrate/op-ir.schema.json`. A schema change that adds /
// renames a variant or token FAILS here, forcing the manual transcription to be
// updated in lockstep (so the ergonomics types cannot silently rot vs the
// contract). The golden `.ir.json` corpus remains the authoritative contract.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(here, "../../../crates/zeroship-migrate/op-ir.schema.json");
const schema = JSON.parse(await readFile(schemaPath, "utf8"));

/** The `const` tokens of a `oneOf` string-enum def. */
function enumTokens(def: any): string[] {
  return def.oneOf.map((b: any) => b.const).filter((c: any) => typeof c === "string").sort();
}
/** The internally-tagged variant tags of an internally-tagged `oneOf` def
 *  (the const of the `tagField` property in each branch). */
function variantTags(def: any, tagField: string): string[] {
  return def.oneOf
    .map((b: any) => b?.properties?.[tagField]?.const)
    .filter((c: any) => typeof c === "string")
    .sort();
}

// ── The token sets the hand-authored `ir.ts` declares (kept in lockstep). ──

const TS = {
  // Op variant tags (the `del()` fn records `"delete"`).
  Op: [
    "createTable", "dropTable", "addColumn", "dropColumn", "createIndex", "dropIndex",
    "alterColumnType", "alterColumnNullability", "renameColumn", "addConstraint",
    "dropConstraint", "insert", "update", "delete", "backfill",
  ].sort(),
  // Expr node tags.
  Expr: ["colRef", "literal", "binOp", "unaryOp", "case", "fnCall", "fnSynth", "cast"].sort(),
  // ColType string tokens (the object-variant arms — ref/vector/decimal/encrypted
  // — are not `const` and are checked structurally by the round-trip, not here).
  ColTypeStrings: [
    "string", "text", "int", "bigInt", "float", "bool", "json", "timestamp",
    "uuid", "bytea", "geoPoint",
  ].sort(),
  // IrConstraintKind tags.
  IrConstraintKind: ["pk", "fk", "unique", "check"].sort(),
  // The closed string-enums (generated into enums.ts).
  BinaryOp: ["eq", "ne", "lt", "le", "gt", "ge", "and", "or", "add", "sub", "mul", "div", "concat"].sort(),
  UnaryOp: ["not", "isNull", "isNotNull", "isTrue", "isFalse"].sort(),
  ScalarFn: ["coalesce", "nullif", "lower", "upper", "trim", "length", "abs"].sort(),
  SynthFn: ["concatWs", "splitPart", "now", "genRandomUuid"].sort(),
  SynthDefaultFn: ["now", "genRandomUuid"].sort(),
  CastTarget: ["text", "integer", "real", "boolean", "blob"].sort(),
  IndexMethod: ["btree", "gin", "gist", "ivfflat", "hnsw", "fts5"].sort(),
};

test("Op variant tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.Op, "op"), TS.Op);
});

test("Expr node tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.Expr, "node"), TS.Expr);
});

test("ColType string tokens match the schema", () => {
  assert.deepEqual(enumTokens(schema.$defs.ColType), TS.ColTypeStrings);
});

test("IrConstraintKind tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.IrConstraintKind, "kind"), TS.IrConstraintKind);
});

test("closed string-enum tokens match the schema", () => {
  for (const name of ["BinaryOp", "UnaryOp", "ScalarFn", "SynthFn", "SynthDefaultFn", "CastTarget", "IndexMethod"] as const) {
    assert.deepEqual(enumTokens(schema.$defs[name]), (TS as any)[name], `${name} tokens drifted`);
  }
});
