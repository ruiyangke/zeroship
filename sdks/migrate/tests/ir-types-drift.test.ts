// Drift guard for the hand-authored IR structural types (`src/generated/ir.ts`)
// + the generated enum tokens (`src/generated/enums.ts`): every `Op` variant tag,
// every `Expr` node tag, the `ColType` token set, and every closed string-enum
// token in the TS types is pinned against the engine's single-source-of-truth
// schema `third_party/zero-migrate/crates/zero-migrate/ir-envelope.schema.json`. A schema change that adds /
// renames a variant or token FAILS here, forcing the manual transcription to be
// updated in lockstep (so the ergonomics types cannot silently rot vs the
// contract). The golden `.ir.json` corpus remains the authoritative contract.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(here, "../../../third_party/zero-migrate/crates/zero-migrate/ir-envelope.schema.json");
const schema = JSON.parse(await readFile(schemaPath, "utf8"));

/** The `const` tokens of a `oneOf` string-enum def. */
function enumTokens(def: any): string[] {
  return def.oneOf.map((b: any) => b.const).filter((constant: any) => typeof constant === "string").sort();
}
/** The internally-tagged variant tags of an internally-tagged `oneOf` def
 *  (the const of the `tagField` property in each branch). */
function variantTags(def: any, tagField: string): string[] {
  return def.oneOf
    .map((b: any) => b?.properties?.[tagField]?.const)
    .filter((constant: any) => typeof constant === "string")
    .sort();
}

/** Map an `Op` variant tag → the set of property NAMES the schema declares for
 *  that variant. Drives the per-op field-presence drift gate (PR10 review F4: the
 *  hand-authored `ir.ts` lost `schema`/`existenceGuard` and kept a removed
 *  `ifExists` — a tag-only gate would not have caught it). */
function opFieldsByTag(def: any, tagField: string): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  for (const b of def.oneOf) {
    const tag = b?.properties?.[tagField]?.const;
    if (typeof tag === "string") {
      out[tag] = Object.keys(b.properties ?? {}).filter((k) => k !== tagField).sort();
    }
  }
  return out;
}

// ── The token sets the hand-authored `ir.ts` declares (kept in lockstep). ──

const TS = {
  // Op variant tags (the `delete()` fn records `"delete"`).
  Op: [
    "createTable", "createPartition", "attachPartition", "detachPartition", "dropPartition",
    "dropTable", "renameTable", "addColumn", "dropColumn", "createIndex",
    "dropIndex", "setColumnType", "setColumnNotNull", "dropColumnNotNull",
    "setColumnDefault", "dropColumnDefault", "renameColumn",
    "alterPrimaryKey", "synchronizeIdentity", "addConstraint",
    "setTableOptions", "dropConstraint", "validateConstraint", "insert", "update", "delete", "backfill", "dialectal", "createView", "dropView",
    "createEnum", "dropEnum", "createDomain", "dropDomain", "createSequence",
    "alterSequence", "dropSequence", "createTrigger", "dropTrigger",
    "createSchema", "dropSchema", "createExtension", "dropExtension", "createRole",
    "alterRole", "dropRole", "dropOwnedBy", "grant", "revoke", "setRls",
    "createPolicy", "dropPolicy", "createFunction",
    "dropFunction", "comment", "pgRaw",
  ].sort(),
  // Expr node tags.
  Expr: [
    "colRef", "literal", "binOp", "unaryOp", "case", "fnCall", "fnSynth",
    "uuidV4", "uuidV7", "cast",
    "between", "like", "distinctFrom", "agg",
    "inList", "pgRegexMatch", "pgColumnSize", "extract", "pgExtract", "pgInterval",
    "dialect",
  ].sort(),
  // ColType string tokens (the object-variant arms — char/ref/vector/decimal/encrypted
  // — are not `const` and are checked structurally by the round-trip, not here).
  ColTypeStrings: [
    "text", "int", "smallInt", "bigInt", "double", "real", "boolean",
    "json", "timestamp", "date", "uuid", "inet", "textArray", "bytes", "geoPoint",
  ].sort(),
  // IrConstraintKind tags.
  IrConstraintKind: ["fk", "unique", "check", "exclusion"].sort(),
  // §A2 — trigger action/body tags.
  TriggerAction: ["executeFunction", "body"].sort(),
  TriggerStmt: ["insert", "update", "delete", "select", "raise"].sort(),
  // §A1 — structured view body tags.
  ViewQuery: ["structured", "raw"].sort(),
  SelectItem: ["colRef", "expr"].sort(),
  OrderItem: ["colRef", "expr"].sort(),
  GrantTarget: ["table", "schema", "sequence", "database"].sort(),
  IndexElement: ["column", "expr"].sort(),
  CommentTarget: ["table", "column", "index", "constraint", "view", "type", "sequence", "function"].sort(),
  // The closed string-enums (generated into enums.ts).
  BinaryOp: ["eq", "ne", "lt", "le", "gt", "ge", "and", "or", "add", "sub", "mul", "div", "concat"].sort(),
  UnaryOp: ["not", "isNull", "isNotNull", "isTrue", "isFalse"].sort(),
  ScalarFn: ["coalesce", "nullif", "lower", "upper", "trim", "length", "abs", "mod", "round", "floor", "ceil", "substr", "replace", "currentSetting", "currentUser"].sort(),
  // There is no UUID `SynthFn` token: UUID generation is carried by the dedicated
  // `uuidV4`/`uuidV7` Expr nodes, so it is absent from this closed set.
  SynthFn: ["concatWs", "splitPart", "now"].sort(),
  CastTarget: ["text", "int", "real", "boolean", "bytes", "uuid"].sort(),
  ExtractField: ["year", "month", "day", "hour", "minute", "dow"].sort(),
  PgExtractField: [
    "second", "doy", "epoch", "quarter", "week", "isodow", "isoyear",
    "century", "decade", "millennium", "microseconds", "milliseconds",
    "timezone", "timezone_hour", "timezone_minute",
  ].sort(),
  AggFunc: ["count", "sum", "avg", "min", "max", "stringAgg", "arrayAgg", "boolAnd", "boolOr"].sort(),
  IndexSortOrder: ["asc", "desc"].sort(),
  IndexMethod: ["btree", "brin", "gin", "gist", "ivfflat", "hnsw", "fts5"].sort(),
  PartitionSpec: ["hash", "list", "range"].sort(),
  PartitionBounds: ["default", "hash", "list", "range"].sort(),
  PartitionBoundValue: ["int", "maxValue", "minValue", "string"].sort(),
  ExclusionMethod: ["gist", "spgist", "btree"].sort(),
  ExclusionOperator: ["&&", "=", "<>", "<", ">", "<=", ">="].sort(),
  // **PR10** — the closed existence-guard token set (`ir.ts` `ExistenceGuard`).
  ExistenceGuard: ["ifNotExists", "ifExists"].sort(),
  // **C1** — the closed FK referential-action token set (`ir.ts` `RefAction`).
  RefAction: ["cascade", "restrict", "setNull", "setDefault", "noAction"].sort(),
  TriggerTiming: ["before", "after", "insteadOf"].sort(),
  TriggerEvent: ["insert", "update", "delete", "truncate"].sort(),
  ForEach: ["row", "statement"].sort(),
  RaiseLevel: ["abort", "fail", "ignore", "rollback"].sort(),
  JoinKind: ["inner", "left"].sort(),
  OrderDir: ["asc", "desc"].sort(),
  Privilege: [
    "all", "select", "insert", "update", "delete", "truncate", "references",
    "trigger", "usage", "connect", "create", "execute", "temporary",
  ].sort(),
  PolicyCmd: ["all", "select", "insert", "update", "delete"].sort(),
  FuncArgMode: ["in", "out", "inout"].sort(),
  FuncLanguage: ["plpgsql", "sql"].sort(),
  FuncVolatility: ["volatile", "stable", "immutable"].sort(),
  TableStrictness: ["strict", "lenient", "off"].sort(),
};

// **PR10 review F4** — the per-`Op` FIELD-presence map the hand-authored `ir.ts`
// declares, pinned against the schema's per-variant `properties` (minus the `op`
// tag). This is what catches the field-level drift a tag-only gate misses: the
// removed native `ifExists`, and the added `schema?` / `existenceGuard?`. If the
// schema gains/loses a field on any op, THIS fails — forcing `ir.ts` to be
// regenerated in lockstep. Sorted; the `op` discriminant is excluded.
const TS_OP_FIELDS: Record<string, string[]> = {
  createTable: ["columns", "constraints", "existenceGuard", "indexes", "name", "partitionBy", "primaryKey", "runtimeOptions", "schema"].sort(),
  createPartition: ["bounds", "existenceGuard", "name", "of", "schema"].sort(),
  attachPartition: ["bound", "name", "parent", "schema"].sort(),
  detachPartition: ["concurrently", "name", "parent", "schema"].sort(),
  dropPartition: ["cascade", "existenceGuard", "name", "parent", "schema"].sort(),
  dropTable: ["cascade", "existenceGuard", "schema", "table"].sort(),
  renameTable: ["existenceGuard", "schema", "table", "to"].sort(),
  // #173/#174 + generated/identity — addColumn carries the column facets that are
  // sound on an added column (NOT `idPrefix`: an added column is never the system PK).
  addColumn: ["caseSensitive", "column", "default", "existenceGuard", "generated", "identity", "mask", "nullable", "schema", "table", "type", "valueFormat", "vectorMetric"].sort(),
  dropColumn: ["column", "existenceGuard", "schema", "table"].sort(),
  createIndex: ["columns", "concurrently", "existenceGuard", "include", "name", "nullsNotDistinct", "only", "schema", "table", "unique", "using", "where", "with"].sort(),
  dropIndex: ["concurrently", "existenceGuard", "name", "schema", "table", "unique"].sort(),
  setColumnType: ["column", "existenceGuard", "schema", "table", "toType", "using"].sort(),
  setColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  dropColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  setColumnDefault: ["column", "existenceGuard", "schema", "table", "value"].sort(),
  dropColumnDefault: ["column", "existenceGuard", "schema", "table"].sort(),
  renameColumn: ["existenceGuard", "from", "schema", "table", "to", "type"].sort(),
  // Explicit PK lifecycle + imported-identity generator reconciliation. Neither
  // carries an `existenceGuard`: both assert their live precondition exactly.
  alterPrimaryKey: ["action", "schema", "table"].sort(),
  synchronizeIdentity: ["column", "schema", "table", "writesQuiesced"].sort(),
  setTableOptions: ["options", "schema", "table"].sort(),
  addConstraint: ["constraint", "existenceGuard", "schema", "table"].sort(),
  dropConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  validateConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  // DML ops carry `schema` but NO `existenceGuard`.
  insert: ["columns", "onConflict", "rows", "schema", "table"].sort(),
  update: ["schema", "set", "table", "where"].sort(),
  delete: ["limit", "schema", "table", "where"].sort(),
  backfill: ["batchSize", "cursorColumns", "cursorStability", "filter", "name", "schema", "set", "table"].sort(),
  dialectal: ["default", "mysql", "pg", "sqlite"].sort(),
  createView: ["columns", "materialized", "name", "query", "replace", "schema"].sort(),
  dropView: ["existenceGuard", "materialized", "name", "schema"].sort(),
  createEnum: ["name", "schema", "values"].sort(),
  dropEnum: ["existenceGuard", "name", "schema"].sort(),
  createDomain: ["as", "check", "default", "name", "notNull", "schema"].sort(),
  dropDomain: ["existenceGuard", "name", "schema"].sort(),
  createSequence: ["as", "cache", "cycle", "increment", "maxValue", "minValue", "name", "ownedBy", "schema", "start"].sort(),
  alterSequence: ["cache", "cycle", "increment", "maxValue", "minValue", "name", "ownedBy", "restart", "schema"].sort(),
  dropSequence: ["existenceGuard", "name", "schema"].sort(),
  createTrigger: ["action", "events", "forEach", "name", "schema", "table", "timing", "when"].sort(),
  dropTrigger: ["ifExists", "name", "schema", "table"].sort(),
  createSchema: ["authorization", "ifNotExists", "name"].sort(),
  dropSchema: ["cascade", "ifExists", "name"].sort(),
  createExtension: ["ifNotExists", "name", "schema"].sort(),
  dropExtension: ["ifExists", "name"].sort(),
  createRole: [
    "bypassRls", "createDb", "createRole", "ifNotExists", "inRole", "login",
    "name", "password", "setSearchPath", "superuser",
  ].sort(),
  alterRole: ["name", "resetSearchPath", "setSearchPath"].sort(),
  dropRole: ["ifExists", "name"].sort(),
  dropOwnedBy: ["roles"].sort(),
  grant: ["on", "privileges", "to", "withGrantOption"].sort(),
  revoke: ["from", "on", "privileges"].sort(),
  setRls: ["enabled", "forced", "schema", "table"].sort(),
  createPolicy: ["forCmd", "name", "schema", "table", "to", "using", "withCheck"].sort(),
  dropPolicy: ["ifExists", "name", "schema", "table"].sort(),
  createFunction: ["args", "body", "language", "name", "replace", "returns", "schema", "volatility"].sort(),
  dropFunction: ["argTypes", "ifExists", "name", "schema"].sort(),
  comment: ["comment", "target"].sort(),
  pgRaw: ["reason", "sql"].sort(),
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

test("trigger action/body tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.TriggerAction, "kind"), TS.TriggerAction);
  assert.deepEqual(variantTags(schema.$defs.TriggerStmt, "stmt"), TS.TriggerStmt);
});

test("view query/body tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.ViewQuery, "kind"), TS.ViewQuery);
  assert.deepEqual(variantTags(schema.$defs.SelectItem, "kind"), TS.SelectItem);
  assert.deepEqual(variantTags(schema.$defs.OrderItem, "kind"), TS.OrderItem);
});

test("vendor grant target tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.GrantTarget, "kind"), TS.GrantTarget);
});

test("index element and comment target tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.IndexElement, "kind"), TS.IndexElement);
  assert.deepEqual(variantTags(schema.$defs.CommentTarget, "kind"), TS.CommentTarget);
});

test("partition type tags match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.PartitionSpec, "kind"), TS.PartitionSpec);
  assert.deepEqual(variantTags(schema.$defs.PartitionBounds, "kind"), TS.PartitionBounds);
  assert.deepEqual(variantTags(schema.$defs.PartitionBoundValue, "kind"), TS.PartitionBoundValue);
});

// **#180** — the migration flags-override FIELD set. `IrFlagsOverride` in `ir.ts`
// is a flat struct (no `op` tag), so the per-`Op` field gate below does not cover
// it — that is exactly how `lock_timeout_ms` was added to the engine schema but
// silently never transcribed into `ir.ts`. Pin its property set against the schema
// so a future added/renamed flag FAILS here, forcing `ir.ts` into lockstep.
const TS_FLAGS_OVERRIDE = [
  "transactional", "destructive", "online", "requires_approval", "repeatable",
  "engine_goodie_ddl", "timeout_ms", "lock_timeout_ms", "phase",
].sort();

test("IrFlagsOverride field set matches the schema (#180)", () => {
  assert.deepEqual(
    Object.keys(schema.$defs.IrFlagsOverride.properties).sort(),
    TS_FLAGS_OVERRIDE,
    "IrFlagsOverride field set drifted from the schema — update src/generated/ir.ts",
  );
});

test("safe integer schema bounds match the hand-authored IR mirror", () => {
  assert.deepEqual(
    {
      safeI64: {
        minimum: schema.$defs.SafeI64.minimum,
        maximum: schema.$defs.SafeI64.maximum,
      },
      safeU64: {
        minimum: schema.$defs.SafeU64.minimum,
        maximum: schema.$defs.SafeU64.maximum,
      },
    },
    {
      safeI64: { minimum: -9_007_199_254_740_991, maximum: 9_007_199_254_740_991 },
      safeU64: { minimum: 0, maximum: 9_007_199_254_740_991 },
    },
    "SafeI64/SafeU64 bounds drifted from src/generated/ir.ts number aliases and SDK recorder validation",
  );
});

test("closed string-enum tokens match the schema", () => {
  for (const name of [
    "BinaryOp",
    "UnaryOp",
    "ScalarFn",
    "SynthFn",
    "CastTarget",
    "ExtractField",
    "PgExtractField",
    "AggFunc",
    "IndexSortOrder",
    "IndexMethod",
    "ExclusionMethod",
    "ExclusionOperator",
    "TriggerTiming",
    "TriggerEvent",
    "ForEach",
    "RaiseLevel",
    "JoinKind",
    "OrderDir",
    "Privilege",
    "PolicyCmd",
    "FuncArgMode",
    "FuncLanguage",
    "FuncVolatility",
    "TableStrictness",
  ] as const) {
    assert.deepEqual(enumTokens(schema.$defs[name]), (TS as any)[name], `${name} tokens drifted`);
  }
});

test("ExistenceGuard tokens match the schema (PR10)", () => {
  assert.deepEqual(enumTokens(schema.$defs.ExistenceGuard), TS.ExistenceGuard);
});

test("RefAction tokens match the schema (C1 FK actions)", () => {
  assert.deepEqual(enumTokens(schema.$defs.RefAction), TS.RefAction);
});

// **PR10 review F4** — the per-op FIELD-presence drift gate. This is the gate the
// stale-`ir.ts` review asked for: had it existed, the un-regenerated `ir.ts` (a
// removed `ifExists`, a missing `schema`/`existenceGuard`) would have FAILED CI
// here, never shipping a wrong public wire type.
test("every Op variant's field set matches the schema (no removed/missing fields)", () => {
  const schemaFields = opFieldsByTag(schema.$defs.Op, "op");
  // Same variant tags on both sides (covered above, re-asserted as a precondition).
  assert.deepEqual(Object.keys(schemaFields).sort(), Object.keys(TS_OP_FIELDS).sort());
  for (const tag of Object.keys(schemaFields)) {
    assert.deepEqual(
      schemaFields[tag],
      TS_OP_FIELDS[tag],
      `Op "${tag}" field set drifted from the schema — regenerate src/generated/ir.ts`,
    );
  }
});

test("createTable primaryKey is present in the schema and hand-authored ir.ts", () => {
  const schemaFields = opFieldsByTag(schema.$defs.Op, "op");
  assert.ok(schemaFields.createTable.includes("primaryKey"));
  const irTs = readFileSync(resolve(here, "../src/generated/ir.ts"), "utf8");
  assert.match(irTs, /primaryKey:\s*string\[\]\s*\|\s*null/);
});

// **PR10** — explicit assertion that the removed native `ifExists` is GONE from
// the schema (the intentional wire break), so a future re-introduction is caught.
test("legacy guardable Op variants do not carry the removed native ifExists field", () => {
  const schemaFields = opFieldsByTag(schema.$defs.Op, "op");
  for (const [tag, fields] of Object.entries(schemaFields)) {
    if (![
      "dropTable",
      "dropPartition",
      "renameTable",
      "dropColumn",
      "dropIndex",
      "setColumnType",
      "setColumnNotNull",
      "dropColumnNotNull",
      "setColumnDefault",
      "dropColumnDefault",
      "renameColumn",
      "dropConstraint",
      "dropView",
    ].includes(tag)) {
      continue;
    }
    assert.ok(!fields.includes("ifExists"), `Op "${tag}" must not carry the removed ifExists`);
  }
});

// ── Export-name ↔ $defs coverage (orphan / omitted structural-type gate). ──
//
// The checks above pin FIELD-level detail for a hand-picked set of NAMED
// types (`TS_OP_FIELDS`, `TS_FLAGS_OVERRIDE`, the enum-token lists). None of
// them catch a type that has no business being in this file at all: an
// exported interface whose name (and claimed shape) has no counterpart
// anywhere in the schema. That is exactly how `IrBatch` — a
// `{ cursorColumn: string; batchSize: number }` the runtime REJECTS at
// `ops.ts` ('backfill({ cursorColumn }) is not a field of the backfill op;
// use cursorColumns: ["column"]') — survived here: it even carried its own
// doc comment asserting "SDK-LOCAL ergonomics ... the engine schema has no
// matching $defs entry", a claim nothing enforced and that was itself false
// (nothing constructed or consumed it; the real, validated field is the
// PLURAL `cursorColumns` already on the `backfill` `Op` variant above).
//
// This check is a NAME correspondence, not a structural one: for every
// `export interface X` / `export type X` declared directly in `ir.ts`, the
// (possibly renamed) name must be a key of the schema's `$defs` — or, for the
// one name that IS legitimately the document root rather than a `$defs`
// entry (`MigrationIr`), match the schema's `title`.
//
// WHAT THIS DOES NOT CATCH (read before trusting a green run here):
//  - Field-shape drift on a type that keeps a VALID name. A type whose name
//    matches a `$defs` key but whose fields silently diverge from it is NOT
//    caught here unless it is also one of the hand-picked sets checked above
//    (`TS_OP_FIELDS` for `Op` variants, `TS_FLAGS_OVERRIDE` for
//    `IrFlagsOverride`, the enum-token lists). Most exported interfaces in
//    this file (`IrIndex`, `IrColumn`, `TableRuntimeOptions`, …) have NO
//    field-level check anywhere in this file.
//  - Nested/inline shapes used only inside another type (e.g. the `perRow`
//    arm of `BackfillSetValue`) beyond the outer name's existence.
//  - The closed string-enum `$defs` (`BinaryOp`, `RefAction`, …): those are
//    `import type`-ed from `./enums.js`, not declared with `export
//    interface`/`export type` in THIS file, so the regex extraction below
//    does not see them. They are not silently unchecked, though — the
//    "closed string-enum tokens match the schema" test above already pins
//    every one of those, by name, individually.
//  - The `RENAMED_TO_SCHEMA` map is a manually maintained, INTENTIONAL
//    allowlist of the two cases where the TS name legitimately differs from
//    the schema `$defs` key. It is not a general escape hatch: a future
//    orphan type must NOT be "fixed" by adding it to this map — it has no
//    schema counterpart to rename to.
const RENAMED_TO_SCHEMA: Record<string, string> = {
  MaskKind: "IrMaskKind",
  Classification: "IrClassification",
};
const ROOT_TYPE = "MigrationIr"; // document root: checked against schema.title, not $defs.

test("every exported interface/type ir.ts declares has a schema counterpart (no orphan types like IrBatch)", () => {
  const irTs = readFileSync(resolve(here, "../src/generated/ir.ts"), "utf8");
  const exported = [...irTs.matchAll(/^export (?:interface|type) (\w+)/gm)].map((m) => m[1]);
  // Sanity floor so a regex/extraction regression (e.g. the export style
  // changes and the pattern stops matching) reads as a fast, loud failure
  // here rather than a silent "0 orphans found because nothing was scanned".
  assert.ok(exported.length > 40, `sanity: expected dozens of exported ir.ts types, got ${exported.length}: ${exported.join(", ")}`);
  assert.equal(schema.title, ROOT_TYPE, "schema root title changed — update ROOT_TYPE below");

  const defs = schema.$defs;
  const orphans: string[] = [];
  for (const name of exported) {
    if (name === ROOT_TYPE) continue;
    const schemaName = RENAMED_TO_SCHEMA[name] ?? name;
    if (!Object.prototype.hasOwnProperty.call(defs, schemaName)) orphans.push(name);
  }
  assert.deepEqual(
    orphans,
    [],
    `ir.ts exports type(s) with no schema $defs counterpart — an orphan SDK-local type asserting a shape ` +
      `nothing in the engine schema backs (this is how IrBatch shipped): ${orphans.join(", ")}`,
  );
});

// **PR10 review F4** — the "regenerate + diff" freshness gate. Re-run the codegen
// (`gen-ir-types.mjs`) into a temp file and assert byte-equality with the committed
// `src/generated/enums.ts`. A schema change that alters a generated enum (e.g. a new
// `ExistenceGuard` token) without re-running the codegen FAILS here — the committed
// generated artifact can never silently go stale vs `ir-envelope.schema.json`.
test("committed generated/enums.ts is up to date with the schema (regenerate + diff)", () => {
  const genScript = resolve(here, "../scripts/gen-ir-types.mjs");
  const committed = resolve(here, "../src/generated/enums.ts");
  const out = join(mkdtempSync(join(tmpdir(), "zs-genir-")), "enums.ts");
  execFileSync(process.execPath, [genScript], { env: { ...process.env, GEN_IR_OUT: out } });
  assert.equal(
    readFileSync(out, "utf8"),
    readFileSync(committed, "utf8"),
    "src/generated/enums.ts is stale — run `pnpm --filter @zeroship/migrate gen:ir-types`",
  );
});
