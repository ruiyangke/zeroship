/**
 * Schema normalization: converts Mongoose-style schema definitions and TypeBuilder
 * instances into a unified NormalizedSchema used by the rest of the SDK.
 */
import { TypeBuilder, SchemaBuilder, FieldDef, FieldDefaultValue, PrimitiveTypeName, PlainObject } from "./types.js";

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

type MongooseConstructor =
  | typeof String
  | typeof Number
  | typeof Boolean
  | typeof Date
  | typeof Object;

interface MongooseFieldDef {
  type: MongooseConstructor | MongooseConstructor[];
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: FieldDefaultValue | (() => FieldDefaultValue);
  min?: number;
  max?: number;
  minlength?: number;
  maxlength?: number;
  enum?: (string | number)[];
  match?: RegExp;
}

type SchemaInput = Record<string, MongooseFieldDef | TypeBuilder<any, any> | MongooseConstructor | [MongooseConstructor]>;

/**
 * C2 — collection-level discriminated union. When the value of a key
 * in `createDb({...})` is `t.union(...)` directly, the collection is a
 * discriminated union: every row is one of the declared variants. The
 * SDK expands this into flat columns per the proposal (§C2) — see
 * `normalizeSchema` / `expandUnionToFlatColumns`.
 */
type UnionSchemaInput = TypeBuilder<any, any>;

/** Accepts either a record of fields OR a top-level union TypeBuilder. */
type SchemaInputOrUnion = SchemaInput | UnionSchemaInput;

/** Maps a Mongoose constructor (String, Number, …) to our internal PrimitiveTypeName. */
function mapConstructorType(ctor: MongooseConstructor): PrimitiveTypeName {
  switch (ctor) {
    case String:
      return "string";
    case Number:
      return "number";
    case Boolean:
      return "boolean";
    case Date:
      return "date";
    case Object:
      return "json";
    default:
      return "string";
  }
}

/** Returns true when the set of valid bare constructor values matches. */
function isBareConstructor(val: unknown): val is MongooseConstructor {
  return (
    val === String ||
    val === Number ||
    val === Boolean ||
    val === Date ||
    val === Object
  );
}

/**
 * Returns true when the value is a single-element array whose element is a bare
 * constructor — the Mongoose shorthand for an array field: `[String]`.
 */
function isBareArrayConstructor(val: unknown): val is [MongooseConstructor] {
  return (
    Array.isArray(val) &&
    val.length === 1 &&
    isBareConstructor(val[0])
  );
}

/** Returns true when the value looks like a Mongoose-style field definition object. */
function isMongooseFieldDef(val: unknown): val is MongooseFieldDef {
  if (val === null || typeof val !== "object") return false;
  const v = val as Record<string, unknown>;
  return (
    "type" in v &&
    (v.type === String ||
      v.type === Number ||
      v.type === Boolean ||
      v.type === Date ||
      v.type === Object ||
      Array.isArray(v.type))
  );
}

/**
 * @internal
 * Converts a SchemaInput (Mongoose-style or TypeBuilder) into a NormalizedSchema.
 *
 * Handles three input forms per field:
 *   1. TypeBuilder instance — `t.string().required()`
 *   2. Mongoose object def — `{ type: String, required: true }`
 *   3. Bare constructor shorthand — `String`, `Number`, `[String]`
 *
 * Fields that match none of these forms are silently skipped.
 */
export function normalizeSchema(input: SchemaInputOrUnion): NormalizedSchema {
  // C2 — top-level discriminated union. The whole collection's row
  // shape is a `t.union(...)`, expanded into flat columns + discriminator.
  if (input instanceof TypeBuilder) {
    const def = input.toFieldDef();
    if (def.type === "union") {
      return expandUnionToFlatColumns(def);
    }
    throw new Error(
      `normalizeSchema: top-level TypeBuilder must be a t.union(...) (got type "${def.type}")`,
    );
  }
  const result: NormalizedSchema = {};

  for (const [key, rawVal] of Object.entries(input)) {
    const val = rawVal;

    if (val instanceof TypeBuilder) {
      // Form 1: TypeBuilder instance — already structurally complete,
      // including the B2 ref* fields (refTarget, onDelete, onUpdate,
      // deferrable). Shallow-clone is correct: every member is a plain
      // value or a function (`default`), never a mutable container.
      result[key] = { ...val.toFieldDef() };
    } else if (isMongooseFieldDef(val)) {
      // Form 2: Mongoose object definition { type: Constructor, ... }
      const mdef = val as MongooseFieldDef;
      const fieldDef: FieldDef = {} as FieldDef;

      if (Array.isArray(mdef.type)) {
        const itemCtor = mdef.type[0] as MongooseConstructor;
        fieldDef.type = "array";
        fieldDef.items = mapConstructorType(itemCtor);
      } else {
        fieldDef.type = mapConstructorType(mdef.type as MongooseConstructor);
      }

      if (mdef.required !== undefined) fieldDef.required = mdef.required;
      if (mdef.unique !== undefined) fieldDef.unique = mdef.unique;
      if (mdef.index !== undefined) fieldDef.index = mdef.index;
      if (mdef.default !== undefined) fieldDef.default = mdef.default;
      if (mdef.min !== undefined) fieldDef.min = mdef.min;
      if (mdef.max !== undefined) fieldDef.max = mdef.max;
      if (mdef.minlength !== undefined) fieldDef.min = mdef.minlength;
      if (mdef.maxlength !== undefined) fieldDef.max = mdef.maxlength;
      if (mdef.enum !== undefined) fieldDef.enum = mdef.enum;
      if (mdef.match !== undefined) fieldDef.pattern = mdef.match;

      result[key] = fieldDef;
    } else if (isBareArrayConstructor(val)) {
      // Form 3b: bare array shorthand — [String], [Number], etc.
      result[key] = { type: "array", items: mapConstructorType(val[0]) };
    } else if (isBareConstructor(val)) {
      // Form 3a: bare constructor shorthand — String, Number, Boolean, Date, Object
      result[key] = { type: mapConstructorType(val) };
    } else {
      throw new Error(`unrecognized schema field "${key}": expected a type constructor (String, Number, ...), { type: Constructor }, or t.string()/t.number()/...`);
    }
  }

  return result;
}

/**
 * C2 — expand a top-level `t.union(...)` into a flat `NormalizedSchema`.
 *
 * - Each non-discriminator field that appears in any variant becomes a
 *   top-level nullable column.
 * - Fields that appear in multiple variants must agree on their `type`;
 *   nullability is the **loosest** (a field required in one variant
 *   and optional in another collapses to optional at the column level,
 *   since at least one variant can produce a row without it). Per-
 *   variant required-ness is preserved in the variant shape so the
 *   DDL emitter can add CHECK constraints (e.g.
 *   `kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)`).
 * - The discriminator field becomes a NOT NULL column with an `enum`
 *   constraint listing every variant's literal value. The field
 *   also carries `variants` (the raw per-variant shape map) and
 *   `discriminator: "__discriminator__"` so the DDL emitter / diff
 *   engine know it's a flat-expanded union.
 */
export function expandUnionToFlatColumns(def: FieldDef): NormalizedSchema {
  if (def.type !== "union" || def.variants === undefined || def.discriminator === undefined) {
    throw new Error("expandUnionToFlatColumns: not a union FieldDef");
  }
  const discriminator = def.discriminator;
  const variants = def.variants;
  const result: NormalizedSchema = {};

  // 1. Discriminator column. The literal value of `discriminator` in
  //    each variant becomes a member of the enum. The discriminator's
  //    primitive type is inferred from the first variant's literal —
  //    every variant must agree (validated below).
  const discValues: (string | number | boolean)[] = [];
  let discPrimType: "string" | "number" | "boolean" | null = null;
  for (let i = 0; i < variants.length; i++) {
    const fd = variants[i][discriminator];
    if (fd === undefined || fd.type !== "literal" || fd.literalValue === undefined) {
      throw new Error(
        `expandUnionToFlatColumns: variant #${i} missing discriminator field "${discriminator}"`,
      );
    }
    const lit = fd.literalValue;
    const primTy = typeof lit;
    if (primTy !== "string" && primTy !== "number" && primTy !== "boolean") {
      throw new Error(
        `expandUnionToFlatColumns: discriminator literal of variant #${i} has unsupported type "${primTy}"`,
      );
    }
    if (discPrimType === null) {
      discPrimType = primTy as "string" | "number" | "boolean";
    } else if (discPrimType !== primTy) {
      throw new Error(
        `expandUnionToFlatColumns: discriminator literals across variants must share a primitive type (got "${discPrimType}" and "${primTy}")`,
      );
    }
    discValues.push(lit);
  }
  // Defensive: detectDiscriminator already enforces distinctness, but
  // the wire-format variant store is JSON-cloned in places; re-check.
  const seen = new Set<string>();
  for (const v of discValues) {
    const tag = typeof v + ":" + String(v);
    if (seen.has(tag)) {
      throw new Error(
        `expandUnionToFlatColumns: duplicate discriminator value ${JSON.stringify(v)}`,
      );
    }
    seen.add(tag);
  }

  result[discriminator] = {
    type: discPrimType ?? "string",
    required: true,
    enum: discValues as (string | number)[],
    discriminator: "__discriminator__",
    variants: variants.map((v) => {
      const cloned: Record<string, FieldDef> = {};
      for (const [k, fd] of Object.entries(v)) cloned[k] = { ...fd };
      return cloned;
    }),
  };

  // 2. All non-discriminator fields. Each variant's fields become
  //    nullable top-level columns; fields shared across variants must
  //    agree on `type`. We never propagate `required` from a variant
  //    to the column level since not every row will populate it.
  for (const variant of variants) {
    for (const [field, fd] of Object.entries(variant)) {
      if (field === discriminator) continue;
      const existing = result[field];
      if (existing === undefined) {
        // Strip variant-only required marker — at the column level the
        // field is nullable. Per-variant required-ness lives in the
        // discriminator's `variants` and surfaces as CHECK constraints.
        const expanded: FieldDef = { ...fd, required: false };
        result[field] = expanded;
      } else {
        if (existing.type !== fd.type) {
          throw new Error(
            `expandUnionToFlatColumns: field "${field}" has incompatible types across variants ("${existing.type}" vs "${fd.type}")`,
          );
        }
        // Existing already nullable — nothing to do.
      }
    }
  }

  return result;
}

/**
 * B2 — verify that every `t.ref("table")` in `schemas` points at a
 * collection that is itself declared in `schemas`. Throws an `Error`
 * whose `message` is a JSON-encoded envelope with code
 * `ref_target_not_found` on the first violation.
 *
 * The check is intentionally string-based (not type-based) so it acts
 * as a safety net for `t.ref("x" as any)` escapes that bypass the
 * compile-time `Tables<S>` constraint. Cross-app refs are also blocked:
 * the TS literal type for `table` always refers to a key inside the
 * same `createDb({...})` literal, so a cross-app reference would have
 * to be smuggled in via `as any`, which this check catches.
 */
export function validateRefTargets(
  schemas: Record<string, unknown>,
): void {
  const declaredCollections = new Set(Object.keys(schemas));
  const reportMissing = (
    collection: string,
    field: string,
    target: string,
  ): never => {
    throw new Error(
      JSON.stringify({
        code: "ref_target_not_found",
        collection,
        field,
        target,
        message:
          `t.ref("${target}") on ${collection}.${field} — ` +
          `target collection "${target}" is not declared in createDb(). ` +
          `Add "${target}" to the schema map, or fix the typo.`,
      }),
    );
  };

  for (const [collectionName, rawSchema] of Object.entries(schemas)) {
    // C2 — a top-level `t.union(...)` walks the variants for refs.
    if (rawSchema instanceof TypeBuilder) {
      const fd = rawSchema.toFieldDef();
      if (fd.type === "union" && fd.variants !== undefined) {
        for (const variant of fd.variants) {
          for (const [field, vDef] of Object.entries(variant)) {
            if (vDef.type === "ref" && vDef.refTarget !== undefined && !declaredCollections.has(vDef.refTarget)) {
              reportMissing(collectionName, field, vDef.refTarget);
            }
          }
        }
      }
      continue;
    }
    const fields =
      rawSchema instanceof SchemaBuilder
        ? (rawSchema as SchemaBuilder<Record<string, unknown>>).fields
        : rawSchema;
    if (fields === null || typeof fields !== "object") continue;
    for (const [field, def] of Object.entries(fields as PlainObject)) {
      let refTarget: string | undefined;
      if (def instanceof TypeBuilder) {
        const fd = def.toFieldDef();
        if (fd.type === "ref") refTarget = fd.refTarget;
      } else if (
        def !== null &&
        typeof def === "object" &&
        "type" in (def as PlainObject) &&
        (def as PlainObject).type === "ref"
      ) {
        // Raw FieldDef literal (rare path — `as any` escape).
        refTarget = (def as { refTarget?: string }).refTarget;
      }
      if (refTarget !== undefined && !declaredCollections.has(refTarget)) {
        reportMissing(collectionName, field, refTarget);
      }
    }
  }
}
