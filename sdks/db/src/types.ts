/**
 * Core type definitions and the `t` type-builder API for @appbase/db.
 * Use `t.string()`, `t.number()`, etc. to declare schema fields with optional
 * constraints, then pass the result to `model()`.
 */

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
  _def: FieldDef;

  constructor(def: FieldDef) {
    this._def = { ...def };
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
    const itemType = items._def.type as PrimitiveTypeName;
    return new TypeBuilder({ type: "array", items: itemType });
  },
};
