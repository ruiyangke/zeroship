/**
 * Core type definitions and the `t` type-builder API for @zeroship/db.
 * Use `t.string()`, `t.number()`, etc. to declare schema fields with optional
 * constraints, then pass the result to `model()`.
 */

/** Generic plain object type used throughout the SDK. */
export type PlainObject = Record<string, unknown>;

/** PostgreSQL transaction isolation levels. */
export type IsolationLevel = "read uncommitted" | "read committed" | "repeatable read" | "serializable";

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
  T extends readonly (infer U)[] ? InferType<U>[] :
  unknown;

/** Infers the value type from a field definition (Mongoose-style, bare constructor, or builder). */
export type InferFieldDef<T> =
  T extends { type: infer U } ? InferType<U> :
  T extends StringConstructor | NumberConstructor | BooleanConstructor | DateConstructor | ObjectConstructor ? InferType<T> :
  T extends readonly (infer U)[] ? InferType<U>[] :
  T extends TypeBuilder<infer U, any> ? U :
  unknown;

/** Keys that are explicitly marked required: true in the field definition or via TypeBuilder.required(). */
export type RequiredKeys<S> = {
  [K in keyof S]:
    S[K] extends { required: true } ? K :
    S[K] extends TypeBuilder<any, true> ? K :
    never
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

/** Input type accepted by `create()` — required fields are required, auto-generated fields excluded. */
export type CreateInput<S> = InferSchema<S> & {
  id?: never;
  createdAt?: never;
  updatedAt?: never;
};

// ---------------------------------------------------------------------------
// Filter types — typed query operators per field type
// ---------------------------------------------------------------------------

/** Comparison operators available on any field type. */
type ComparisonOps<T> = {
  $eq?: T;
  $ne?: T | null;
  $gt?: T;
  $gte?: T;
  $lt?: T;
  $lte?: T;
  $in?: T[];
  $nin?: T[];
  $exists?: boolean;
};

/** String-specific operators. */
type StringOps = {
  $like?: string;
  $ilike?: string;
  $search?: string;
};

/** Filter value for a field — either a direct value, null, or operator object. */
type FilterValue<T> =
  T | null |
  (NonNullable<T> extends string ? ComparisonOps<NonNullable<T>> & StringOps :
   NonNullable<T> extends number ? ComparisonOps<NonNullable<T>> :
   NonNullable<T> extends boolean ? ComparisonOps<NonNullable<T>> :
   ComparisonOps<NonNullable<T>>);

/** Typed filter for a document — each field accepts its value type or operators. */
export type Filter<S> = {
  [K in keyof Document<S>]?: FilterValue<Document<S>[K]>
} & {
  $and?: Filter<S>[];
  $or?: Filter<S>[];
  $not?: Filter<S>;
};

// ---------------------------------------------------------------------------
// Update expression types — typed operators per field type
// ---------------------------------------------------------------------------

/** Numeric update operators. */
type NumericUpdateOps = {
  $inc?: number;
  $dec?: number;
  $mul?: number;
};

/** Array update operators. */
type ArrayUpdateOps<T> = {
  $push?: T;
  $pull?: T;
  $addToSet?: T;
};

/** Update value for a single field — direct value or typed operator. */
type UpdateFieldValue<T> =
  T |
  (NonNullable<T> extends number ? NumericUpdateOps : never) |
  (NonNullable<T> extends readonly unknown[] ? ArrayUpdateOps<NonNullable<T>[number]> : never);

/** Typed update expression — per-field operators. */
export type UpdateExpression<S> = {
  [K in keyof InferSchema<S>]?: UpdateFieldValue<InferSchema<S>[K]>
} & {
  // Mongoose top-level operators (SDK translates to per-field)
  $set?: Partial<InferSchema<S>>;
  $inc?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends number ? number : never };
  $dec?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends number ? number : never };
  $mul?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends number ? number : never };
  $push?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
  $pull?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
  $addToSet?: { [K in keyof InferSchema<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
};

// ---------------------------------------------------------------------------
// Naming strategy — maps JS field names ↔ PG column names
// ---------------------------------------------------------------------------

/** Bidirectional mapping between JS field names and database column names. */
export interface NamingStrategy {
  /** Convert a JS field name to a database column name: `firstName` → `first_name` */
  toColumn(field: string): string;
  /** Convert a database column name to a JS field name: `first_name` → `firstName` */
  toField(column: string): string;
}

/** Built-in naming strategies. */
export const naming = {
  /** camelCase → snake_case (industry standard, default) */
  snakeCase: {
    toColumn: (s: string) => s.replace(/[A-Z]/g, c => '_' + c.toLowerCase()),
    toField: (s: string) => s.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()),
  } satisfies NamingStrategy,
  /** Pass-through — field names used as-is (requires quoted identifiers in PG) */
  asIs: {
    toColumn: (s: string) => s,
    toField: (s: string) => s,
  } satisfies NamingStrategy,
};

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

/** Union of all values that can serve as a field default. */
export type FieldDefaultValue = string | number | boolean | Date | null | PlainObject | string[] | number[] | boolean[];

/** Internal representation of a fully-specified field definition used by validate and collection. */
export interface FieldDef {
  type: TypeName;
  items?: PrimitiveTypeName;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: FieldDefaultValue | (() => FieldDefaultValue);
  min?: number;
  max?: number;
  enum?: (string | number)[];
  pattern?: RegExp;
}

/**
 * Fluent builder for a single field definition.
 * Generic params: T = the inferred TS value type, R = whether required.
 * `t.string().required().min(3).max(50)` → TypeBuilder<string, true>
 */
export class TypeBuilder<T = unknown, R extends boolean = false> {
  /** @internal Type-level brand — do not access at runtime. */
  declare readonly _type: T;
  /** @internal Type-level brand for required/optional distinction. */
  declare readonly _required: R;

  private _def: FieldDef;

  constructor(def: FieldDef) {
    this._def = { ...def };
  }

  /** Returns a frozen copy of the field definition. */
  toFieldDef(): Readonly<FieldDef> {
    return Object.freeze({ ...this._def });
  }

  /** Marks the field as required; validation will fail if the field is absent. */
  required(): TypeBuilder<T, true> {
    this._def.required = true;
    return this as unknown as TypeBuilder<T, true>;
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
  default(val: FieldDefaultValue | (() => FieldDefaultValue)): this {
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
  enum(...values: (string | number)[]): this {
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
  string(): TypeBuilder<string> {
    return new TypeBuilder<string>({ type: "string" });
  },
  /** Creates a number field definition. */
  number(): TypeBuilder<number> {
    return new TypeBuilder<number>({ type: "number" });
  },
  /** Creates a boolean field definition. */
  boolean(): TypeBuilder<boolean> {
    return new TypeBuilder<boolean>({ type: "boolean" });
  },
  /** Creates a date field definition (accepts `Date` objects or ISO date strings). */
  date(): TypeBuilder<number> {
    return new TypeBuilder<number>({ type: "date" });
  },
  /** Creates a JSON/object field definition for arbitrary nested data. */
  json(): TypeBuilder<Record<string, unknown>> {
    return new TypeBuilder<Record<string, unknown>>({ type: "json" });
  },
  /**
   * Creates an array field definition. Pass the item type builder as the argument:
   * `t.array(t.string())` produces `{ type: "array", items: "string" }`.
   */
  array<U>(items: TypeBuilder<U, any>): TypeBuilder<U[]> {
    const itemType = items.toFieldDef().type as PrimitiveTypeName;
    return new TypeBuilder<U[]>({ type: "array", items: itemType });
  },
};

// ---------------------------------------------------------------------------
// Schema builder — per-collection options via fluent API
// ---------------------------------------------------------------------------

/** Options that can be set per-collection via the schema() builder. */
export interface SchemaOptions {
  softDelete: boolean;
}

/**
 * Wraps field definitions with per-collection options.
 * Use `schema({ ... }).softDelete()` to enable soft delete for a specific collection.
 */
export class SchemaBuilder<S> {
  fields: S;
  options: SchemaOptions;

  constructor(fields: S) {
    this.fields = fields;
    this.options = { softDelete: false };
  }

  /** Enable soft delete — deleteOne/deleteMany set `deletedAt` instead of removing rows. */
  softDelete(): this {
    this.options.softDelete = true;
    return this;
  }
}

/** Create a schema with per-collection options. */
export function schema<S>(fields: S): SchemaBuilder<S> {
  return new SchemaBuilder(fields);
}
