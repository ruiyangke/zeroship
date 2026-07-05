// Drift guard for the hand-authored IR structural types (`src/generated/ir.ts`)
// + the generated enum tokens (`src/generated/enums.ts`): every `Op` variant tag,
// every `Expr` node tag, the `ColType` token set, and every closed string-enum
// token in the TS types is pinned against the engine's single-source-of-truth
// schema `crates/zeroship-migrate/op-ir.schema.json`. A schema change that adds /
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
  // Op variant tags (the `del()` fn records `"delete"`).
  Op: [
    "createTable", "createPartition", "detachPartition", "dropPartition",
    "dropTable", "renameTable", "addColumn", "dropColumn", "createIndex",
    "dropIndex", "setColumnType", "setColumnNotNull", "dropColumnNotNull",
    "setColumnDefault", "dropColumnDefault", "renameColumn", "addConstraint",
    "setTableOptions", "dropConstraint", "validateConstraint", "insert", "update", "delete", "backfill", "createView", "dropView",
    "createEnum", "dropEnum", "createDomain", "dropDomain", "createSequence",
    "alterSequence", "dropSequence", "createTrigger", "dropTrigger",
    "createSchema", "dropSchema", "createExtension", "dropExtension", "createRole",
    "alterRole", "dropRole", "dropOwnedBy", "grant", "revoke", "enableRls", "forceRls",
    "disableRls", "noForceRls", "createPolicy", "dropPolicy", "createFunction",
    "dropFunction", "comment", "pgRaw",
  ].sort(),
  // Expr node tags.
  Expr: [
    "colRef", "literal", "binOp", "unaryOp", "case", "fnCall", "fnSynth", "cast",
    "between", "like", "distinctFrom", "agg",
    "pgArrayMembership", "pgRegexMatch", "pgColumnSize", "extract", "pgIntervalLiteral",
    "dialect",
  ].sort(),
  // ColType string tokens (the object-variant arms — char/ref/vector/decimal/encrypted
  // — are not `const` and are checked structurally by the round-trip, not here).
  ColTypeStrings: [
    "string", "text", "int", "smallInt", "bigInt", "double", "real", "boolean",
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
  SynthFn: ["concatWs", "splitPart", "now", "genRandomUuid"].sort(),
  SynthDefaultFn: ["now", "genRandomUuid"].sort(),
  CastTarget: ["text", "integer", "real", "boolean", "blob", "uuid"].sort(),
  PgArrayMembershipOp: ["eq", "ne"].sort(),
  ExtractField: ["day"].sort(),
  AggFunc: ["count", "sum", "avg", "min", "max"].sort(),
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
  detachPartition: ["concurrently", "name", "parent", "schema"].sort(),
  dropPartition: ["cascade", "existenceGuard", "name", "schema"].sort(),
  dropTable: ["cascade", "existenceGuard", "schema", "table"].sort(),
  renameTable: ["existenceGuard", "schema", "table", "to"].sort(),
  // #173/#174 + generated/identity — addColumn carries the column facets that are
  // sound on an added column (NOT `idPrefix`: an added column is never the system PK).
  addColumn: ["caseSensitive", "column", "default", "existenceGuard", "generated", "identity", "mask", "nullable", "schema", "table", "type", "vectorMetric"].sort(),
  dropColumn: ["column", "existenceGuard", "schema", "table"].sort(),
  createIndex: ["columns", "concurrently", "existenceGuard", "include", "name", "nullsNotDistinct", "only", "schema", "table", "unique", "using", "where", "with"].sort(),
  dropIndex: ["concurrently", "existenceGuard", "name", "schema", "table", "unique"].sort(),
  setColumnType: ["column", "existenceGuard", "schema", "table", "toType", "using"].sort(),
  setColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  dropColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  setColumnDefault: ["column", "existenceGuard", "schema", "table", "value"].sort(),
  dropColumnDefault: ["column", "existenceGuard", "schema", "table"].sort(),
  renameColumn: ["existenceGuard", "from", "schema", "table", "to", "type"].sort(),
  setTableOptions: ["options", "schema", "table"].sort(),
  addConstraint: ["constraint", "existenceGuard", "schema", "table"].sort(),
  dropConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  validateConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  // DML ops carry `schema` but NO `existenceGuard`.
  insert: ["columns", "onConflict", "rows", "schema", "table"].sort(),
  update: ["batch", "schema", "set", "table", "where"].sort(),
  delete: ["limit", "schema", "table", "where"].sort(),
  backfill: ["batchSize", "cursorColumn", "filter", "name", "schema", "set", "table"].sort(),
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
  enableRls: ["schema", "table"].sort(),
  forceRls: ["schema", "table"].sort(),
  disableRls: ["schema", "table"].sort(),
  noForceRls: ["schema", "table"].sort(),
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
    "SynthDefaultFn",
    "CastTarget",
    "PgArrayMembershipOp",
    "ExtractField",
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

// **PR10 review F4** — the "regenerate + diff" freshness gate. Re-run the codegen
// (`gen-ir-types.mjs`) into a temp file and assert byte-equality with the committed
// `src/generated/enums.ts`. A schema change that alters a generated enum (e.g. a new
// `ExistenceGuard` token) without re-running the codegen FAILS here — the committed
// generated artifact can never silently go stale vs `op-ir.schema.json`.
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
