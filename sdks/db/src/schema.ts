/**
 * Schema normalization: converts Mongoose-style schema definitions and TypeBuilder
 * instances into a unified NormalizedSchema used by the rest of the SDK.
 */
import { TypeBuilder, SchemaBuilder, FieldDef, FieldDefaultValue, PrimitiveTypeName } from "./types.js";

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
export function normalizeSchema(input: SchemaInput): NormalizedSchema {
  const result: NormalizedSchema = {};

  for (const [key, rawVal] of Object.entries(input)) {
    const val = rawVal;

    if (val instanceof TypeBuilder) {
      // Form 1: TypeBuilder instance
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
