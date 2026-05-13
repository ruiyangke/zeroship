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
 * The persisted document type: user fields + auto-generated `id`, `createdAt`, `updatedAt`.
 * Extends InferSchema so required fields remain required.
 *
 * `version` is included as optional because `SchemaBuilder.withVersioning()`
 * (D4) injects it at DDL time. Collections without versioning will never
 * see it populated; collections with versioning treat it as a CAS guard
 * key in filters (see `Collection.updateOne` and the `_extractCasVersion`
 * helper).
 */
export type Document<S> = InferSchema<S> & {
  id: number;
  createdAt: number;
  updatedAt: number;
  version?: number;
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
export type PrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json" | "calendarDate";
/** Definition for an array field with a declared item type. */
export type ArrayTypeDef = { type: "array"; items: PrimitiveTypeName };
/**
 * All supported type names. Includes "array", "ref" (B2 typed FK),
 * "object" (D2 nested validators), and "calendarDate" (D3 — `YYYY-MM-DD`).
 */
export type TypeName = PrimitiveTypeName | "array" | "ref" | "object";

/** Union of all values that can serve as a field default. */
export type FieldDefaultValue = string | number | boolean | Date | null | PlainObject | string[] | number[] | boolean[];

/**
 * Foreign-key action policy for `t.ref()` (proposal B2).
 *
 * - `restrict`  — refuse to delete the parent row if any child references it.
 *                 This is the **default** per proposal R1 (`docs/proposals/zeroship-db-v2.md`
 *                 around line 762): silent cascading deletes are catastrophic
 *                 data-loss, so opt-in cascade is the safer default.
 * - `cascade`   — child rows are deleted/updated along with the parent.
 * - `set null`  — child reference column is nulled when parent is deleted.
 *                 Only valid when the column is nullable.
 * - `no action` — like `restrict` but check is deferrable (Postgres default
 *                 inside DEFERRABLE constraints).
 */
export type FkAction = "restrict" | "cascade" | "set null" | "no action";

/**
 * Options accepted by `t.ref()` to control FK behaviour at the DB layer.
 */
export interface RefOptions {
  /** ON DELETE policy. Default: "restrict". */
  onDelete?: FkAction;
  /** ON UPDATE policy. Default: "restrict". */
  onUpdate?: FkAction;
  /**
   * Emit `DEFERRABLE INITIALLY DEFERRED` for this FK so the constraint
   * check is queued until COMMIT (lets circular refs be inserted in any
   * order within one tx). Default: `true` — flips per proposal B2's
   * "Deferred-constraint cost" caveat which currently keeps it on.
   */
  deferrable?: boolean;
}

/**
 * Cross-table typed ID (B2). Stored as an integer at the DB layer but
 * brand-tagged at the type layer so `Id<"users">` and `Id<"posts">`
 * are mutually incompatible — typos like
 * `db.posts.findOne({ authorId: postId })` (where `postId` is `Id<"posts">`)
 * become compile errors.
 *
 * Modelled after Convex's `Id<TableName>` brand
 * ([docs.convex.dev/database/document-ids]). The brand is a phantom
 * property typed but never assigned at runtime; the runtime value is
 * just a number, so JSON serialisation is unchanged.
 */
export type Id<T extends string> = number & {
  readonly __zeroshipTable: T;
};

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
  /** Target table name for `t.ref()`. Present iff `type === "ref"`. */
  refTarget?: string;
  /** ON DELETE policy for `t.ref()`. Default at DDL emit time: "restrict". */
  onDelete?: FkAction;
  /** ON UPDATE policy for `t.ref()`. Default at DDL emit time: "restrict". */
  onUpdate?: FkAction;
  /**
   * Whether the FK is emitted DEFERRABLE INITIALLY DEFERRED. Default at
   * DDL emit time: true (see RefOptions.deferrable).
   */
  deferrable?: boolean;
  /**
   * Nested-object shape (D2). Present iff `type === "object"`. The value
   * is a normalised sub-schema — each entry maps a JS field name to a
   * `FieldDef`. The DB column is JSONB; validation recurses into the
   * shape and reports errors using a dotted path (e.g. `profile.bio`).
   */
  shape?: Record<string, FieldDef>;
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
 * const fields = {
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
  /**
   * Creates a foreign-key field referencing `table` (B2). At the type
   * level produces `TypeBuilder<Id<T>>` so consumers get a brand-typed
   * `Id<"users">` rather than a bare `number`. At the DB level it
   * materialises a `FOREIGN KEY (<column>) REFERENCES "<schema>"."<table>"(id)`
   * constraint with the default `ON DELETE RESTRICT` policy (proposal R1).
   *
   * `opts.onDelete` / `opts.onUpdate` override the policy, e.g.:
   * ```ts
   * { authorId: t.ref("users", { onDelete: "cascade" }) }
   * ```
   *
   * `opts.deferrable` (default true) emits `DEFERRABLE INITIALLY DEFERRED`
   * so circular references can be inserted in any order within one tx.
   */
  ref<T extends string>(table: T, opts?: RefOptions): TypeBuilder<Id<T>> {
    if (typeof table !== "string" || table.length === 0) {
      throw new Error("t.ref(table) requires a non-empty table name");
    }
    return new TypeBuilder<Id<T>>({
      type: "ref",
      refTarget: table,
      onDelete: opts?.onDelete ?? "restrict",
      onUpdate: opts?.onUpdate ?? "restrict",
      deferrable: opts?.deferrable ?? true,
    });
  },
  /**
   * D2 — nested-object validator. The argument is a record of nested
   * field declarations (each a `TypeBuilder`, including another
   * `t.object()` for arbitrary depth). Storage is a JSONB column;
   * validation recurses into the shape and reports errors using a
   * dotted path like `profile.social.twitter`.
   *
   * ```ts
   * profile: t.object({
   *   bio:    t.string().max(500),
   *   social: t.object({
   *     twitter: t.string().optional(),
   *   }),
   * }),
   * ```
   *
   * Type inference: `Document<S>["profile"]["social"]["twitter"]` is
   * `string | undefined` — the same rules as the top-level schema apply
   * recursively (`required()` keeps a key required, otherwise optional).
   */
  object<S extends Record<string, TypeBuilder<any, any>>>(shape: S): TypeBuilder<InferSchema<S>> {
    if (shape === null || typeof shape !== "object" || Array.isArray(shape)) {
      throw new Error("t.object(shape) requires a record of nested type builders");
    }
    const nested: Record<string, FieldDef> = {};
    for (const [key, val] of Object.entries(shape)) {
      if (!(val instanceof TypeBuilder)) {
        throw new Error(`t.object: nested field "${key}" must be a TypeBuilder (use t.string(), t.number(), ...)`);
      }
      nested[key] = { ...val.toFieldDef() };
    }
    return new TypeBuilder<InferSchema<S>>({ type: "object", shape: nested });
  },
  /**
   * D3 — calendar-date validator. Accepts a `YYYY-MM-DD` string and
   * stores it as a Postgres `DATE` column (no time, no timezone). This
   * is distinct from `t.date()` which is a `TIMESTAMPTZ` stored as
   * Unix-ms numbers at the JS layer.
   *
   * ```ts
   * birthday: t.calendarDate(),
   * ```
   */
  calendarDate(): TypeBuilder<string> {
    return new TypeBuilder<string>({ type: "calendarDate" });
  },
};

// ---------------------------------------------------------------------------
// Schema builder — per-collection options via fluent API
// ---------------------------------------------------------------------------

/**
 * Per-collection strictness for deploy-time data validation
 * (proposal @zeroship/db v2, section A2). The default is `strict` to
 * match Convex's `schemaValidation: true` default.
 *
 * - `strict`  — refuse the deploy on any validation violation. The
 *   worker returns a `validation_refused` envelope and the SDK throws
 *   at module-init time so the app fails fast.
 * - `lenient` — log violations but allow the push (warning only).
 * - `off`     — skip validation entirely (equivalent of Convex's
 *   `schemaValidation: false`); intended for legacy/imported data.
 */
export type Strictness = "strict" | "lenient" | "off";

/** Options that can be set per-collection via the schema() builder. */
export interface SchemaOptions {
  softDelete: boolean;
  strictness: Strictness;
  /**
   * D4 — optimistic concurrency. When `true` the collection auto-injects
   * an `INTEGER NOT NULL DEFAULT 1` `version` column at DDL time and
   * `updateOne`/`updateMany` honour a `{ version: N }` filter clause for
   * compare-and-swap updates (mismatch returns an
   * `optimistic_lock_failure` error).
   */
  versioning: boolean;
}

/**
 * Wraps field definitions with per-collection options.
 * Use `schema({ ... }).softDelete()` to enable soft delete for a specific collection.
 */
export class SchemaBuilder<S> {
  readonly fields: S;
  private _options: SchemaOptions;

  constructor(fields: S) {
    this.fields = fields;
    this._options = { softDelete: false, strictness: "strict", versioning: false };
  }

  /** Returns the collection options. */
  get options(): Readonly<SchemaOptions> { return this._options; }

  /** Enable soft delete — deleteOne/deleteMany set `deletedAt` instead of removing rows. */
  softDelete(): this {
    this._options.softDelete = true;
    return this;
  }

  /**
   * D4 — enable optimistic concurrency. Auto-injects a `version` column
   * (INTEGER NOT NULL DEFAULT 1) at DDL time. Update calls that include
   * `{ version: N }` in the filter become compare-and-swap: rows are
   * updated and `version` is incremented only when the stored version
   * matches N. A mismatch returns
   * `{ data: null, error: { code: "optimistic_lock_failure" } }`.
   */
  withVersioning(): this {
    this._options.versioning = true;
    return this;
  }

  /**
   * Set the deploy-time data-validation strictness for this collection
   * (A2 of the @zeroship/db v2 proposal). Default is `strict`.
   *
   * - `strict`  — refuse the push on any violation.
   * - `lenient` — warn but allow.
   * - `off`     — skip validation entirely.
   */
  strictness(level: Strictness): this {
    this._options.strictness = level;
    return this;
  }
}

/** Create a schema with per-collection options. */
export function schema<S>(fields: S): SchemaBuilder<S> {
  return new SchemaBuilder(fields);
}
