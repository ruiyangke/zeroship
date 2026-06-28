// IR adapter — the JS half of the JS schema front-end (design §5.1).
//
// This module is the runtime-entry glue evaluated inside zeroship-runtime's
// V8 sandbox. It imports the (already-bundled, self-contained) creator
// schema module under the fixed specifier `__schema__.js`, walks the
// `t.*`-built schema map, and lowers each collection to the engine's
// `CollectionDescriptor` JSON — the descriptor IR the lean
// `zeroship-migrate` differ consumes (`desired_snapshot` /
// `DeclarativeAuthor::diff`). It then stashes the serialized IR on
// `globalThis.__zsSchemaIR` for the Rust host to read back.
//
// Why an explicit adapter (not `installSchema`): `@zeroship/bootstrap`'s
// `installSchema` is welded to the native `env.db` platform handle (it
// calls `registerModel` on a live DbPlugin). At CLI/generate time there is
// no db plugin and no app boot — we want a PURE lowering. So this adapter
// reuses only the parts that are pure: `TypeBuilder.toFieldDef()` (the
// canonical wire shape) and `normalizeSchema` (the same field-record
// normaliser `installSchema` uses internally), then translates the SDK
// `FieldDef` field names to the engine's `FieldDescriptor` names
// (`refTarget` -> `ref`, etc.). The translation is the IR boundary, the
// analog of an Atlas ORM provider emitting the internal representation.

import * as userSchemaMod from "./__schema__.js";
import { TypeBuilder } from "@zeroship/db";

// Resolve the schema map from an optional authoring module. This is a
// migration-generation input, not the deploy contract.
function resolveSchemaMap(mod) {
  const def = mod && mod.default;
  if (def && typeof def === "object" && def.schema && typeof def.schema === "object") {
    return def.schema;
  }
  if (mod && mod.schema && typeof mod.schema === "object") {
    return mod.schema;
  }
  throw new Error(
    "schema front-end: the module exports no schema authoring object",
  );
}

// Translate one SDK `FieldDef` (the `toFieldDef()` wire shape) to the
// engine's `FieldDescriptor` JSON. The engine deserializes via serde with
// `#[serde(rename = ...)]`, so the KEY NAMES here are load-bearing — they
// must match `crates/zeroship-migrate/src/declarative.rs::FieldDescriptor`.
function fieldDefToDescriptor(name, def) {
  const out = { name, type: def.type };

  if (def.required === true) out.required = true;
  if (def.unique === true) out.unique = true;

  // t.ref(target, { onDelete, onUpdate, deferrable }) — SDK spells the
  // target `refTarget`; the engine descriptor spells it `ref`.
  if (def.refTarget !== undefined) out.ref = def.refTarget;
  if (def.onDelete !== undefined) out.onDelete = def.onDelete;
  if (def.onUpdate !== undefined) out.onUpdate = def.onUpdate;
  if (def.deferrable !== undefined) out.deferrable = def.deferrable;

  // Constraint goodies — names already match the engine descriptor.
  if (def.literalValue !== undefined) out.literalValue = def.literalValue;
  if (def.default !== undefined && typeof def.default !== "function") {
    out.default = def.default;
  }
  if (def.min !== undefined) out.min = def.min;
  if (def.max !== undefined) out.max = def.max;
  if (def.enum !== undefined) out.enum = def.enum;
  if (def.idPrefix !== undefined) out.idPrefix = def.idPrefix;

  // P2 full-capability facets — carried VERBATIM so the engine's shared
  // `zeroship-schema` kernel maps them identically to plugin-db.
  if (def.vectorDims !== undefined) out.vectorDims = def.vectorDims;
  if (def.vectorMetric !== undefined) out.vectorMetric = def.vectorMetric;
  if (def.encrypted !== undefined) out.encrypted = def.encrypted;
  if (def.mask !== undefined) out.mask = def.mask;

  // T12 full-text search facet — `.fts(language?)` marks a text column for the
  // collection's composite `__fts` tsvector + GIN index. Carried VERBATIM so the
  // engine builds the same `__fts` column / `<coll>__fts_idx` index plugin-db's
  // runtime `fts_search` reads. Previously STRIPPED here, so an FTS field
  // produced a plain text column with no index.
  if (def.fts === true) out.fts = true;
  if (def.ftsLanguage !== undefined) out.ftsLanguage = def.ftsLanguage;

  return out;
}

// Lower one collection's field record to the engine `CollectionDescriptor`.
// `ownerApp` stamps the declaring app (the project-umbrella ownership
// subject); the host passes it in via a global the Rust side sets before
// evaluation.
function collectionToDescriptor(name, fieldRecord, indexes, runtimeOptions, ownerApp) {
  const fields = [];
  for (const [fieldName, builder] of Object.entries(fieldRecord)) {
    if (!(builder instanceof TypeBuilder)) {
      // A non-TypeBuilder value in a field position is a creator error
      // (e.g. a bare string). Surface it loudly rather than silently
      // dropping the column.
      throw new Error(
        `schema front-end: collection "${name}" field "${fieldName}" is not a t.* field`,
      );
    }
    const def = builder.toFieldDef();
    fields.push(fieldDefToDescriptor(fieldName, def));
  }

  const out = { name, owner_app: ownerApp, fields };
  if (runtimeOptions && typeof runtimeOptions === "object") {
    out.runtimeOptions = {
      softDelete: runtimeOptions.softDelete === true,
      versioning: runtimeOptions.versioning === true,
      strictness: runtimeOptions.strictness ?? "strict",
    };
  }
  if (Array.isArray(indexes) && indexes.length > 0) {
    out.indexes = indexes.map((idx) => {
      const d = { name: idx.name, columns: idx.fields ?? idx.columns ?? [] };
      if (idx.unique === true) d.unique = true;
      return d;
    });
  }
  return out;
}

function buildIR(schemaMap, ownerApp) {
  const collections = [];
  for (const [collName, value] of Object.entries(schemaMap)) {
    // A schema entry is either a bare field record `{ <field>: t.* }` or a
    // `{ fields, indexes }` shape (the SchemaBuilder form). Support both.
    let fieldRecord = value;
    let indexes = [];
    let runtimeOptions = undefined;
    if (value && typeof value === "object" && value.fields && typeof value.fields === "object") {
      fieldRecord = value.fields;
      indexes = value.indexes ?? [];
      runtimeOptions = value.options;
    }
    collections.push(collectionToDescriptor(collName, fieldRecord, indexes, runtimeOptions, ownerApp));
  }
  return collections;
}

const ownerApp = (globalThis.__zsOwnerApp && String(globalThis.__zsOwnerApp)) || "app_unknown";
try {
  const schemaMap = resolveSchemaMap(userSchemaMod);
  const ir = buildIR(schemaMap, ownerApp);
  globalThis.__zsSchemaIR = JSON.stringify({ ok: true, collections: ir });
} catch (e) {
  globalThis.__zsSchemaIR = JSON.stringify({
    ok: false,
    error: (e && e.message) ? e.message : String(e),
  });
}

export default {};
