import { TypeBuilder, FieldDef, PrimitiveTypeName } from "./types.js";

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
  default?: unknown;
  min?: number;
  max?: number;
  enum?: unknown[];
  match?: RegExp;
}

type SchemaInput = Record<string, MongooseFieldDef | TypeBuilder | unknown>;

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

export function normalizeSchema(input: SchemaInput): NormalizedSchema {
  const result: NormalizedSchema = {};

  for (const [key, rawVal] of Object.entries(input)) {
    const val = rawVal as unknown;

    if (val instanceof TypeBuilder) {
      result[key] = { ...val._def };
    } else if (isMongooseFieldDef(val)) {
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
      if (mdef.enum !== undefined) fieldDef.enum = mdef.enum;
      if (mdef.match !== undefined) fieldDef.pattern = mdef.match;

      result[key] = fieldDef;
    }
  }

  return result;
}
