/**
 * Core type definitions and the `t` type-builder API for @zeroship/db.
 * Use `t.string()`, `t.number()`, etc. to declare schema fields with optional
 * constraints, then pass the result to `model()`.
 */

/** Generic plain object type used throughout the SDK. */
export type PlainObject = Record<string, unknown>;

/** Return type for all Collection methods. Never throws — errors are values. */
export type Result<T> = { data: T; error: null } | { data: null; error: Error };

// ---------------------------------------------------------------------------
// Schema-to-TypeScript inference utilities
// ---------------------------------------------------------------------------

/** Maps a JS constructor to its corresponding TypeScript primitive type. */
export type InferType<T> =
  T extends StringConstructor ? string :
  T extends NumberConstructor ? number :
  T extends BooleanConstructor ? boolean :
  T extends DateConstructor ? number :        // timestamps stored as Unix ms
  T extends ObjectConstructor ? Record<string, unknown> :
  T extends (infer U)[] ? InferType<U>[] :
  unknown;

/** Infers the value type from a field definition (Mongoose-style, bare constructor, or builder). */
export type InferFieldDef<T> =
  T extends { type: infer U } ? InferType<U> :
  T extends StringConstructor | NumberConstructor | BooleanConstructor | DateConstructor | ObjectConstructor ? InferType<T> :
  T extends (infer U)[] ? InferType<U>[] :
  T extends TypeBuilder ? unknown :           // TypeBuilder inference is best-effort unknown
  unknown;

/** Keys that are explicitly marked required: true in the field definition. */
export type RequiredKeys<S> = {
  [K in keyof S]: S[K] extends { required: true } ? K : never
}[keyof S];

/** Keys that are not explicitly required. */
export type OptionalKeys<S> = Exclude<keyof S, RequiredKeys<S>>;

/**
 * Infers the user-facing shape from a schema definition, honouring required/optional.
 * Required fields are non-optional; all others become `?`.
 */
export type InferSchema<S> = {
  [K in RequiredKeys<S>]: InferFieldDef<S[K]>
} & {
  [K in OptionalKeys<S>]?: InferFieldDef<S[K]>
};

/**
 * The persisted document type: user fields + auto-generated `_id`, `createdAt`, `updatedAt`.
 * Extends InferSchema so required fields remain required.
 */
export type Document<S> = InferSchema<S> & {
  id: number;
  createdAt: number;
  updatedAt: number;
};

/** Input type accepted by `create()` — required fields are required, optional are optional. */
export type CreateInput<S> = InferSchema<S>;

/** Input type accepted by `update*()` — all schema fields become optional. */
export type UpdateInput<S> = Partial<InferSchema<S>>;

/** Wraps a successful value in Result. */
export function ok<T>(data: T): Result<T> {
  return { data, error: null };
}

/** Wraps an error in Result. */
export function err<T>(error: Error): Result<T> {
  return { data: null, error };
}

/** Primitive field type names supported by the SDK. */
export type PrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json";
/** Definition for an array field with a declared item type. */
export type ArrayTypeDef = { type: "array"; items: PrimitiveTypeName };
/** All supported type names, including "array". */
export type TypeName = PrimitiveTypeName | "array";

/** Internal representation of a fully-specified field definition used by validate and collection. */
export interface FieldDef {
  type: TypeName;
  items?: PrimitiveTypeName;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: unknown;
  min?: number;
  max?: number;
  enum?: unknown[];
  pattern?: RegExp;
}

/**
 * Fluent builder for a single field definition.
 * Each method mutates and returns `this`, enabling chaining:
 * `t.string().required().min(3).max(50)`.
 */
export class TypeBuilder {
  private _def: FieldDef;

  constructor(def: FieldDef) {
    this._def = { ...def };
  }

  /** Returns a frozen copy of the field definition. */
  toFieldDef(): Readonly<FieldDef> {
    return Object.freeze({ ...this._def });
  }

  /** Marks the field as required; validation will fail if the field is absent. */
  required(): this {
    this._def.required = true;
    return this;
  }

  /** Adds a unique index constraint to the field. */
  unique(): this {
    this._def.unique = true;
    return this;
  }

  /** Adds a non-unique index to the field for query performance. */
  index(): this {
    this._def.index = true;
    return this;
  }

  /** Sets the default value (or factory function) used when the field is absent on insert. */
  default(val: unknown): this {
    this._def.default = val;
    return this;
  }

  /** For strings: minimum length. For numbers: minimum value. */
  min(n: number): this {
    this._def.min = n;
    return this;
  }

  /** For strings: maximum length. For numbers: maximum value. */
  max(n: number): this {
    this._def.max = n;
    return this;
  }

  /** Restricts the field to a fixed set of allowed values. */
  enum(...values: unknown[]): this {
    this._def.enum = values;
    return this;
  }

  /** For strings: a RegExp the value must match. */
  pattern(re: RegExp): this {
    this._def.pattern = re;
    return this;
  }
}

/**
 * The type-builder namespace. Use these factory functions to define schema fields:
 *
 * ```ts
 * const schema = {
 *   name: t.string().required(),
 *   age:  t.number().min(0),
 *   tags: t.array(t.string()),
 * };
 * ```
 */
export const t = {
  /** Creates a string field definition. */
  string(): TypeBuilder {
    return new TypeBuilder({ type: "string" });
  },
  /** Creates a number field definition. */
  number(): TypeBuilder {
    return new TypeBuilder({ type: "number" });
  },
  /** Creates a boolean field definition. */
  boolean(): TypeBuilder {
    return new TypeBuilder({ type: "boolean" });
  },
  /** Creates a date field definition (accepts `Date` objects or ISO date strings). */
  date(): TypeBuilder {
    return new TypeBuilder({ type: "date" });
  },
  /** Creates a JSON/object field definition for arbitrary nested data. */
  json(): TypeBuilder {
    return new TypeBuilder({ type: "json" });
  },
  /**
   * Creates an array field definition. Pass the item type builder as the argument:
   * `t.array(t.string())` produces `{ type: "array", items: "string" }`.
   */
  array(items: TypeBuilder): TypeBuilder {
    const itemType = items.toFieldDef().type as PrimitiveTypeName;
    return new TypeBuilder({ type: "array", items: itemType });
  },
};
