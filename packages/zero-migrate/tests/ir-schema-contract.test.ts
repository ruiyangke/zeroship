// Semantic checks for the engine's IR schema and the TypeScript generator's
// declarative manifest. TypeScript's compiler tests cover the handwritten public
// types; this suite never parses TypeScript implementation text.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(here, "../../../crates/zeroship-migrate/ir-envelope.schema.json");
const schema = JSON.parse(await readFile(schemaPath, "utf8"));
const manifestPath = resolve(here, "../scripts/ir-type-manifest.json");
const typeManifest = JSON.parse(await readFile(manifestPath, "utf8")) as {
  generated: string[];
  handAuthored: Record<string, string>;
};

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
 *  that variant. Drives the per-op field-presence drift gate (the
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
    "setColumnDefault", "dropColumnDefault", "renameColumn", "alterPrimaryKey", "synchronizeIdentity", "addConstraint",
    "setTableOptions", "dropConstraint", "validateConstraint", "insert", "update", "delete", "backfill", "dialectal", "createView", "dropView",
    "createEnum", "dropEnum", "createDomain", "dropDomain", "createSequence",
    "alterSequence", "dropSequence", "createTrigger", "dropTrigger",
    "createSchema", "dropSchema", "createExtension", "dropExtension", "createRole",
    "alterRole", "dropRole", "dropOwnedBy", "grant", "revoke", "setRls",
    "createPolicy", "dropPolicy", "createFunction",
    "dropFunction", "comment", "raw",
  ].sort(),
  // Expr node tags.
  Expr: [
    "colRef", "literal", "binOp", "unaryOp", "case", "fnCall", "fnSynth", "uuidV4", "uuidV7", "cast",
    "between", "like", "distinctFrom", "agg",
    "inList", "regexMatch", "storageSize", "extract", "interval",
    "dialect",
  ].sort(),
  // ColType string tokens (the object-variant arms — char/ref/vector/decimal/enum/
  // domain — are not `const` and are checked structurally by the round-trip, not here).
  // `string` is now a struct variant (`{ string: { length } }`), not a bare token.
  ColTypeStrings: [
    "text", "int", "bigInt", "double", "boolean",
    "json", "timestamp", "date", "uuid", "inet", "textArray", "bytes", "geoPoint",
  ].sort(),
  // IrConstraintKind tags.
  IrConstraintKind: ["fk", "unique", "check", "exclusion"].sort(),
  // Trigger action/body tags.
  TriggerAction: ["executeFunction", "body"].sort(),
  TriggerStmt: ["insert", "update", "delete", "select", "raise"].sort(),
  // Structured view body tags.
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
  SynthFn: ["concatWs", "splitPart", "now"].sort(),
  CastTarget: ["text", "int", "real", "boolean", "bytes", "uuid"].sort(),
  ExtractField: [
    "year", "month", "day", "hour", "minute", "dow",
    "second", "doy", "epoch", "quarter", "week", "isodow", "isoyear",
    "century", "decade", "millennium", "microseconds", "milliseconds",
    "timezone", "timezoneHour", "timezoneMinute",
  ].sort(),
  AggFunc: ["count", "sum", "avg", "min", "max", "stringAgg", "arrayAgg", "boolAnd", "boolOr"].sort(),
  IndexSortOrder: ["asc", "desc"].sort(),
  IndexMethod: ["btree", "brin", "gin", "gist", "ivfflat", "hnsw"].sort(),
  PartitionSpec: ["hash", "list", "range"].sort(),
  PartitionBounds: ["default", "hash", "list", "range"].sort(),
  PartitionBoundValue: ["int", "maxValue", "minValue", "string"].sort(),
  ExclusionMethod: ["gist", "spgist", "btree"].sort(),
  ExclusionOperator: ["&&", "=", "<>", "<", ">", "<=", ">="].sort(),
  // The closed existence-guard token set (`ir.ts` `ExistenceGuard`).
  ExistenceGuard: ["ifNotExists", "ifExists"].sort(),
  // The closed FK referential-action token set (`ir.ts` `RefAction`).
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
  FuncLanguage: ["procedural", "sql"].sort(),
  FuncVolatility: ["volatile", "stable", "immutable"].sort(),
  TableStrictness: ["strict", "lenient", "off"].sort(),
};

// The per-`Op` FIELD-presence map the hand-authored `ir.ts`
// declares, pinned against the schema's per-variant `properties` (minus the `op`
// tag). This is what catches the field-level drift a tag-only gate misses: the
// removed native `ifExists`, and the added `schema?` / `existenceGuard?`. If the
// schema gains/loses a field on any op, THIS fails — forcing `ir.ts` to be
// regenerated in lockstep. Sorted; the `op` discriminant is excluded.
const TS_OP_FIELDS: Record<string, string[]> = {
  createTable: ["attributes", "columns", "constraints", "existenceGuard", "indexes", "name", "partitionBy", "primaryKey", "runtimeOptions", "schema"].sort(),
  createPartition: ["attributes", "bounds", "existenceGuard", "name", "of", "schema"].sort(),
  attachPartition: ["bound", "name", "parent", "schema"].sort(),
  detachPartition: ["concurrently", "name", "parent", "schema"].sort(),
  dropPartition: ["cascade", "existenceGuard", "name", "parent", "schema"].sort(),
  dropTable: ["cascade", "existenceGuard", "schema", "table"].sort(),
  renameTable: ["existenceGuard", "schema", "table", "to"].sort(),
  // column facets + generated/identity — addColumn carries the column facets that are
  // sound on an added column (NOT `idPrefix`: an added column is never the system PK).
  addColumn: ["attributes", "caseSensitive", "column", "default", "encrypted", "existenceGuard", "generated", "identity", "mask", "nullable", "schema", "table", "type", "vectorMetric"].sort(),
  dropColumn: ["column", "existenceGuard", "schema", "table"].sort(),
  createIndex: ["columns", "concurrently", "existenceGuard", "include", "name", "nullsNotDistinct", "only", "schema", "table", "unique", "using", "where", "attributes"].sort(),
  dropIndex: ["concurrently", "existenceGuard", "name", "schema", "table", "unique"].sort(),
  setColumnType: ["column", "existenceGuard", "schema", "table", "toType", "using"].sort(),
  setColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  dropColumnNotNull: ["column", "existenceGuard", "schema", "table"].sort(),
  setColumnDefault: ["column", "existenceGuard", "schema", "table", "value"].sort(),
  dropColumnDefault: ["column", "existenceGuard", "schema", "table"].sort(),
  renameColumn: ["existenceGuard", "from", "schema", "table", "to", "type"].sort(),
  setTableOptions: ["attributes", "options", "schema", "table"].sort(),
  alterPrimaryKey: ["action", "schema", "table"].sort(),
  synchronizeIdentity: ["column", "schema", "table", "writesQuiesced"].sort(),
  addConstraint: ["attributes", "constraint", "existenceGuard", "schema", "table"].sort(),
  dropConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  validateConstraint: ["existenceGuard", "name", "schema", "table"].sort(),
  // DML ops carry `schema` but NO `existenceGuard`.
  insert: ["columns", "onConflict", "rows", "schema", "table"].sort(),
  update: ["schema", "set", "table", "where"].sort(),
  delete: ["limit", "schema", "table", "where"].sort(),
  backfill: ["batchSize", "cursorColumns", "cursorStability", "filter", "name", "schema", "set", "table"].sort(),
  dialectal: ["legs"],
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
  raw: ["reason", "sql"].sort(),
};

const TS_ALTER_PRIMARY_KEY_FIELDS: Record<string, string[]> = {
  add: ["columns"],
  replace: ["columns", "dropIdentityFrom", "expectedColumns"].sort(),
  drop: ["dropIdentityFrom", "expectedColumns"].sort(),
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

test("AlterPrimaryKeyAction variants and nested fields match the schema", () => {
  assert.deepEqual(variantTags(schema.$defs.AlterPrimaryKeyAction, "kind"), ["add", "drop", "replace"]);
  assert.deepEqual(
    opFieldsByTag(schema.$defs.AlterPrimaryKeyAction, "kind"),
    TS_ALTER_PRIMARY_KEY_FIELDS,
  );
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

// The migration flags-override FIELD set. `IrFlagsOverride` in `ir.ts`
// is a flat struct (no `op` tag), so the per-`Op` field gate below does not cover
// it — that is exactly how `lock_timeout_ms` was added to the engine schema but
// silently never transcribed into `ir.ts`. Pin its property set against the schema
// so a future added/renamed flag FAILS here, forcing `ir.ts` into lockstep.
const TS_FLAGS_OVERRIDE = [
  "transactional", "destructive", "online", "requires_approval", "repeatable",
  "timeout_ms", "lock_timeout_ms", "phase",
].sort();

test("IrFlagsOverride field set matches the schema", () => {
  assert.deepEqual(
    Object.keys(schema.$defs.IrFlagsOverride.properties).sort(),
    TS_FLAGS_OVERRIDE,
    "IrFlagsOverride field set drifted from the schema — update src/generated/ir.ts",
  );
});

test("safe integer schema bounds remain within the JavaScript exact range", () => {
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
    "SafeI64/SafeU64 bounds must remain exactly representable by the SDK",
  );
});

test("the exact int64 scalar carrier uses its canonical wire shape", () => {
  const int64Carrier = schema.$defs.IrScalar.oneOf.find(
    (branch: any) => branch.type === "object" && branch.required?.length === 1 && branch.required[0] === "int64",
  );
  assert.ok(int64Carrier, "IrScalar schema must include the single-key { int64: string } carrier");
  assert.deepEqual(Object.keys(int64Carrier.properties), ["int64"]);
  assert.equal(int64Carrier.properties.int64.type, "string");
  assert.equal(int64Carrier.properties.int64.pattern, "^(0|-?[1-9][0-9]*)$");
  assert.equal(int64Carrier.additionalProperties, false);
});

test("closed string-enum tokens match the schema", () => {
  for (const name of [
    "BinaryOp",
    "UnaryOp",
    "ScalarFn",
    "SynthFn",
    "CastTarget",
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

test("ExistenceGuard tokens match the schema", () => {
  assert.deepEqual(enumTokens(schema.$defs.ExistenceGuard), TS.ExistenceGuard);
});

test("RefAction tokens match the schema (C1 FK actions)", () => {
  assert.deepEqual(enumTokens(schema.$defs.RefAction), TS.RefAction);
});

const HAND_AUTHORED_SCHEMA_ENUMS: Record<string, string[]> = {
  VectorMetric: ["cosine", "innerProduct", "l2"],
  IrMaskKind: ["date-decade", "date-year", "email", "first4", "full", "last4", "name", "none"],
  IrClassification: ["internal", "pci", "phi", "pii", "public", "spi"],
  ColumnCollation: ["bytewise"],
};

test("hand-authored enum contracts match the schema", () => {
  assert.deepEqual(
    Object.keys(typeManifest.handAuthored).sort(),
    Object.keys(HAND_AUTHORED_SCHEMA_ENUMS).sort(),
  );
  for (const [defName, expected] of Object.entries(HAND_AUTHORED_SCHEMA_ENUMS)) {
    assert.deepEqual(enumTokens(schema.$defs[defName]), expected, `${defName} tokens drifted`);
  }
});

test("the type manifest accounts for every closed string enum", () => {
  const closed = Object.entries(schema.$defs)
    .filter(
      ([, def]: [string, any]) =>
        Array.isArray(def.oneOf) &&
        def.oneOf.length > 0 &&
        def.oneOf.every((b: any) => typeof b.const === "string"),
    )
    .map(([name]) => name)
    .sort();
  const declared = [...typeManifest.generated, ...Object.keys(typeManifest.handAuthored)].sort();
  assert.deepEqual(
    declared,
    closed,
    "declare each closed string enum as generated or hand-authored in ir-type-manifest.json",
  );
});

test("every Op variant's field set matches the schema (no removed/missing fields)", () => {
  const schemaFields = opFieldsByTag(schema.$defs.Op, "op");
  // Same variant tags on both sides (covered above, re-asserted as a precondition).
  assert.deepEqual(Object.keys(schemaFields).sort(), Object.keys(TS_OP_FIELDS).sort());
  for (const tag of Object.keys(schemaFields)) {
    assert.deepEqual(
      schemaFields[tag],
      TS_OP_FIELDS[tag],
      `Op "${tag}" field set drifted from the expected wire contract`,
    );
  }
});

test("createTable requires its primary key field", () => {
  const schemaFields = opFieldsByTag(schema.$defs.Op, "op");
  assert.ok(schemaFields.createTable.includes("primaryKey"));
});

test("a typed-id column carries idPrefix on createTable and not on addColumn", () => {
  const irColumnPrefix = schema.$defs.IrColumn.properties.idPrefix;
  assert.ok(irColumnPrefix, "schema must define IrColumn.idPrefix");
  assert.ok(
    Array.isArray(irColumnPrefix.type)
      ? irColumnPrefix.type.includes("string")
      : irColumnPrefix.type === "string",
    "idPrefix is a nullable string",
  );
  assert.equal(schema.$defs.IrColumn.properties.valueFormat, undefined, "valueFormat is retired");

  const addColumn = schema.$defs.Op.oneOf.find(
    (branch: any) => branch.properties?.op?.const === "addColumn",
  );
  assert.ok(addColumn, "schema must define addColumn");
  assert.equal(addColumn.properties.idPrefix, undefined, "addColumn has no idPrefix slot");
  assert.equal(addColumn.properties.valueFormat, undefined, "addColumn has no valueFormat facet");
});

test("per-row generators are represented only in backfill set values", () => {
  const generator = schema.$defs.PerRowGenerator;
  assert.ok(generator, "schema must define PerRowGenerator");
  assert.deepEqual(enumTokens(generator), ["uuidV4", "uuidV7"]);
  const typeId = generator.oneOf.find((branch: any) => branch.required?.includes("typeId"));
  assert.ok(typeId, "PerRowGenerator must include a TypeID prefix arm");
  assert.deepEqual(typeId.required, ["typeId"]);
  assert.deepEqual(typeId.properties.typeId.required, ["prefix"]);
  assert.deepEqual(Object.keys(typeId.properties.typeId.properties), ["prefix"]);

  const backfillSetValue = schema.$defs.BackfillSetValue;
  assert.ok(backfillSetValue, "schema must define BackfillSetValue");
  const perRowArm = backfillSetValue.anyOf.find(
    (branch: any) => branch.properties?.perRow !== undefined,
  );
  assert.ok(perRowArm, "BackfillSetValue must carry an explicit perRow wrapper");
  assert.deepEqual(perRowArm.required, ["perRow"]);
  assert.equal(perRowArm.additionalProperties, false);
  assert.match(JSON.stringify(perRowArm.properties.perRow), /#\/\$defs\/PerRowGenerator/);

  const backfill = schema.$defs.Op.oneOf.find(
    (branch: any) => branch.properties?.op?.const === "backfill",
  );
  const update = schema.$defs.Op.oneOf.find(
    (branch: any) => branch.properties?.op?.const === "update",
  );
  assert.ok(backfill && update, "schema must define backfill and update ops");
  assert.match(JSON.stringify(backfill.properties.set), /#\/\$defs\/BackfillSetValue/);
  assert.match(JSON.stringify(update.properties.set), /#\/\$defs\/IrValue/);
  assert.doesNotMatch(JSON.stringify(update.properties.set), /BackfillSetValue/);
});

// Explicit assertion that the removed native `ifExists` is GONE from
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

// The "regenerate + diff" freshness gate. Re-run the codegen
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
