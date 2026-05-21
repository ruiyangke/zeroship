/**
 * Core type definitions and the `t` type-builder API for @zeroship/db.
 * Use `t.string()`, `t.number()`, etc. to declare schema fields with
 * optional constraints, then export the schema map via
 * `export default { schema: { ... } }`.
 */

/** Generic plain object type used throughout the SDK. */
export type PlainObject = Record<string, unknown>;

/** PostgreSQL transaction isolation levels. */
/**
 * Postgres transaction isolation level. Alias of the ambient
 * `ZeroshipIsolationLevel` from `@zeroship/types/shared.d.ts` so the
 * `db.transaction({ isolationLevel })` option and the procedure
 * `config.isolation` field share one canonical type — drift between
 * the two would be a silent footgun.
 */
export type IsolationLevel = ZeroshipIsolationLevel;

/** Return type for all Collection methods. Never throws — errors are values. */
export type Result<T> = { data: T; error: null } | { data: null; error: Error };

// ---------------------------------------------------------------------------
// Schema-to-TypeScript inference utilities
// ---------------------------------------------------------------------------

/** Infers the value type from a field definition built via `t.*`. */
export type InferFieldDef<T> =
  T extends TypeBuilder<infer U, any> ? U :
  unknown;

/** Keys whose field builder was marked `.required()` (the `R` brand of
 *  `TypeBuilder<_, R>`). */
export type RequiredKeys<S> = {
  [K in keyof S]:
    S[K] extends TypeBuilder<any, true> ? K :
    never
}[keyof S];

/** Keys that are not explicitly required. */
export type OptionalKeys<S> = Exclude<keyof S, RequiredKeys<S>>;

/**
 * True iff every value in S is a `TypeBuilder` (i.e. the input is a
 * schema dictionary, not an already-inferred shape). Used to
 * distinguish a schema dict from an already-inferred shape — the
 * latter appears as the top-level S when `{ events: t.union(...) }`
 * unwraps a TypeBuilder whose `_type` brand is the user-facing union
 * (no TypeBuilders left in the value positions).
 */
type IsSchemaDict<S> =
  S extends Record<string, unknown>
    ? // Pick any value type that's a TypeBuilder. If at least one
      // value is a TypeBuilder we treat S as a schema dict and infer.
      // Otherwise it's already an inferred shape (top-level union
      // variant) and we return S unchanged.
      true extends {
        [K in keyof S]-?: NonNullable<S[K]> extends TypeBuilder<any, any> ? true : false;
      }[keyof S]
      ? true
      : false
    : false;

/**
 * Infers the user-facing shape from a schema definition, honouring
 * required/optional. Required fields are non-optional; all others
 * become `?`.
 *
 * If S is already an inferred shape (no TypeBuilder values — e.g. a
 * top-level union variant after `UnwrapSchema` peels the TypeBuilder
 * brand), return S unchanged so `Row<S>` doesn't strip every
 * field down to `unknown`.
 *
 * Distributes over unions so `InferSchema<A | B>` becomes
 * `InferSchema<A> | InferSchema<B>` — discriminated narrowing then
 * works on the result.
 */
export type InferSchema<S> = S extends infer T
  ? IsSchemaDict<T> extends true
    ? {
        [K in RequiredKeys<T>]: InferFieldDef<T[K]>;
      } & {
        [K in OptionalKeys<T>]?: InferFieldDef<T[K]>;
      }
    : T
  : never;

/**
 * The persisted row type: user fields + auto-generated `id`,
 * `createdAt`, `updatedAt`. Extends `InferSchema` so required fields
 * remain required.
 *
 * `version` is included as optional because `SchemaBuilder.withVersioning()`
 * (D4) injects it at DDL time. Collections without versioning never
 * populate it; versioned collections treat it as a CAS guard key in
 * filters (see `Collection.update` and the `_extractCasVersion` helper).
 */
export type Row<S> = InferSchema<S> & {
  id: number;
  createdAt: number;
  updatedAt: number;
  version?: number;
};

/** Input type accepted by `insert()` / `upsert()` — required fields
 * stay required, auto-generated fields (`id` / `createdAt` /
 * `updatedAt`) are excluded. */
export type RowInput<S> = InferSchema<S> & {
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

/** Typed filter for a document — each field accepts its value type or operators.
 *  Field values use `NonNullable<Row<S>[K]>` so `undefined` is rejected at the
 *  type layer; pass `null` to match SQL NULL explicitly. */
export type Filter<S> = {
  [K in keyof Row<S>]?: FilterValue<NonNullable<Row<S>[K]>>
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

// ---------------------------------------------------------------------------
// Infer<> — type-only helpers that pull `Row` / `RowInput` / `Id` out of a
// Collection without `Parameters<typeof col.insertMany>[0][number]` plumbing.
// ---------------------------------------------------------------------------

/**
 * Generic identity passthrough. Mostly useful so users can write
 * `Infer<typeof db.users.RowInput>` symmetrically with `InferRow<...>` etc.
 * — the input is already the resolved type; this just gives the idiom a
 * single named entry point.
 */
export type Infer<T> = T extends infer X ? X : never;

/** Persisted-row type for a Collection — `Row<S>` for `Collection<S, _>`. */
export type InferRow<C> =
  C extends { readonly _schema_brand?: infer S } ? Row<S> :
  C extends import("./collection.js").Collection<infer S, any> ? Row<S> :
  never;

/** Insert-shape type for a Collection — `RowInput<S>` for `Collection<S, _>`. */
export type InferRowInput<C> =
  C extends { readonly _schema_brand?: infer S } ? RowInput<S> :
  C extends import("./collection.js").Collection<infer S, any> ? RowInput<S> :
  never;

/** Branded `Id<N>` for a Collection — `Id<"users">` for `Collection<_, "users">`. */
export type InferId<C> =
  C extends import("./collection.js").Collection<any, infer N extends string> ? Id<N> :
  never;

/**
 * Spec accepted by `find()` / `get()`'s `with: { ... }` option. Each key
 * must be a `t.ref(...)` field on the parent schema; the value is `true`
 * (eager-load the full target row). Future shapes — column narrowing,
 * relation-level filters — slot in as `{ columns: K[] } | { where: Filter }`.
 */
export type WithSpec = Record<string, true>;

/**
 * Extract the target table name (e.g. `"users"`) from whatever shape the
 * user wrote at `S[K]` for a `t.ref(...)` field. The user-facing schema
 * dict carries a `TypeBuilder<Id<TargetName>>` at that key; the brand
 * tag inside `Id<T>` is the lookup key for `AllSchemas[T]`.
 *
 * Also handles the Mongoose-style raw `{ type: "ref"; refTarget: T }`
 * literal shape so users who skip `t.*` still get strong relation typing.
 *
 * Resolves to `never` when the field at `K` is not a ref — that lets
 * callers surface a "not a t.ref field" error at the type layer.
 */
export type ExtractRefTarget<X> =
  X extends TypeBuilder<infer U, any>
    ? U extends Id<infer T>
      ? T
      : never
    : X extends { type: "ref"; refTarget: infer T extends string }
      ? T
      : never;

/**
 * Unwrap whatever shape an `AllSchemas[name]` slot holds into the raw
 * field-record the `Row<...>` machinery understands. Mirrors the
 * `UnwrapSchema<T>` alias in `db.ts` but lives here so `WithRelations`
 * can call it without importing across the module boundary.
 *
 * - `schema({...})` wraps `Record<string, TypeBuilder>` — strip it.
 * - A top-level `t.union(...)` yields `TypeBuilder<UnionShape>` — strip
 *   to `UnionShape` so the discriminator narrows correctly.
 * - Plain field-record passes through unchanged.
 */
export type UnwrapSchemaForRelation<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any> ? U :
  T;

/**
 * Resolve the target table's `Row<...>` given the field type at `S[K]`
 * and the parent db's schema map. Falls back to `PlainObject` when the
 * target name can't be matched against any declared collection — that
 * preserves the v1 behaviour for unknown targets without breaking
 * compilation. Tightens to the real `Row<TargetSchema>` whenever
 * `_installSchema`'s schema map carries the target name (the common case).
 */
export type ResolveTargetRow<X, AllSchemas> =
  ExtractRefTarget<X> extends infer Target
    ? Target extends keyof AllSchemas
      ? Row<UnwrapSchemaForRelation<AllSchemas[Target]>>
      : Target extends string
        ? PlainObject
        : never
    : never;

/**
 * Type-level shape for joined rows. Each key in `W` becomes a field on
 * the row carrying the target's full `Row<TargetSchema>` (or `null`).
 *
 * `AllSchemas` is the schema map that `_installSchema` was given —
 * threading it through `Collection<S, N, AllSchemas>` lets us look up each key's
 * `t.ref(target)` and resolve `target` to the target collection's `Row`.
 * The default `Record<string, unknown>` keeps direct `Collection`/`Query`
 * users (e.g. `model("users", ...)`) compiling — they degrade to
 * `PlainObject` per relation, exactly the v1 behaviour.
 */
export type WithRelations<S, W extends WithSpec, AllSchemas = Record<string, unknown>> = {
  [K in keyof W & keyof S]: ResolveTargetRow<S[K], AllSchemas> | null;
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
 * "object" (D2 nested validators), "calendarDate" (D3 — `YYYY-MM-DD`),
 * "literal" (C2 discriminator constant), and "union" (C2 discriminated
 * union document shape — proposal §C2).
 */
export type TypeName = PrimitiveTypeName | "array" | "ref" | "object" | "literal" | "union";

/** Union of all values that can serve as a field default. */
export type FieldDefaultValue = string | number | boolean | Date | null | PlainObject | string[] | number[] | boolean[];

/**
 * Foreign-key action policy for `t.ref()` (proposal B2).
 *
 * - `restrict`  — refuse to delete the parent row if any child references it.
 *                 This is the **default** per proposal R1 (`docs/proposals/zeroship-db.md`
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
 * `db.posts.get({ authorId: postId })` (where `postId` is `Id<"posts">`)
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
  /**
   * Literal value (C2). Present iff `type === "literal"`. The accepted
   * value is matched by strict `===`; literal fields are the building
   * block of `t.union()` discriminators (every variant declares its
   * own `kind: t.literal("...")` so the SDK can dispatch at validate
   * time and Postgres can enforce membership with a CHECK constraint).
   */
  literalValue?: string | number | boolean;
  /**
   * Union variants (C2). Present iff `type === "union"`. Each entry is
   * the normalised shape (`Record<string, FieldDef>`) of one variant of
   * a discriminated union. The discriminator key is captured separately
   * in `discriminator`; values for that key are `FieldDef.literalValue`
   * on each variant's discriminator field.
   *
   * Storage strategy is **flat columns** (proposal §C2): every union-
   * wide field becomes a top-level column on the table, plus the
   * discriminator column with a `CHECK (kind IN (...))` constraint.
   * Per-variant integrity is enforced by additional CHECK constraints
   * (`kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)`) so
   * a `kind='login'` row cannot store NULL where the variant requires
   * a value.
   */
  variants?: Record<string, FieldDef>[];
  /**
   * Discriminator field name (C2). Present iff `type === "union"`, or
   * set to `true` on a flat-expanded discriminator column so the DDL
   * emitter knows to attach per-variant CHECK constraints.
   *
   * - On a `type === "union"` FieldDef the value is the discriminator
   *   field name.
   * - On a flat-expanded primitive FieldDef the value is the literal
   *   string `"__discriminator__"` — a sentinel asserting "this is the
   *   discriminator column, and `variants` carries the per-variant
   *   shape map needed for CHECK emission".
   */
  discriminator?: string;
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
  /**
   * Creates a timestamp field — `TIMESTAMPTZ` in Postgres, Unix-ms
   * `number` at the JS layer. Accepts `Date`, ISO string, or `number`
   * on input (the SDK normalises in `validate`). Reads come back as
   * `number` (millisecond epoch). For wall-clock dates without a
   * time-of-day component, use {@link calendarDate} instead.
   */
  timestamp(): TypeBuilder<number> {
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
      throw Object.assign(
        new Error("t.ref(table) requires a non-empty table name"),
        { code: "ref_empty_table" as const },
      );
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
   * Type inference: `Row<S>["profile"]["social"]["twitter"]` is
   * `string | undefined` — the same rules as the top-level schema apply
   * recursively (`required()` keeps a key required, otherwise optional).
   */
  object<S extends Record<string, TypeBuilder<any, any>>>(shape: S): TypeBuilder<InferSchema<S>> {
    if (shape === null || typeof shape !== "object" || Array.isArray(shape)) {
      throw Object.assign(
        new Error("t.object(shape) requires a record of nested type builders"),
        { code: "object_invalid_shape" as const },
      );
    }
    const nested: Record<string, FieldDef> = {};
    for (const [key, val] of Object.entries(shape)) {
      if (!(val instanceof TypeBuilder)) {
        throw Object.assign(
          new Error(`t.object: nested field "${key}" must be a TypeBuilder (use t.string(), t.number(), ...)`),
          { code: "object_field_not_typebuilder" as const },
        );
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
  /**
   * C2 — literal-value field. Validation accepts only the exact value
   * `v` (strict `===`). The value's TS literal type is preserved so
   * `t.literal("login")` yields `TypeBuilder<"login">` and the
   * containing `t.object({ kind: t.literal("login"), ... })` produces
   * an inferred shape with `kind: "login"` rather than `kind: string`.
   *
   * Literals are the building block of `t.union()` discriminators:
   * every variant declares its own `t.literal(<value>)` on the same
   * key, and the SDK auto-detects the discriminator.
   *
   * Storage: a literal-typed field at the top level of a collection is
   * stored as its underlying primitive (TEXT / NUMERIC / BOOLEAN) with
   * a `CHECK (col = '<value>')` constraint. Inside a union, the
   * literal value appears in the discriminator's per-variant CHECK
   * constraint and the discriminator's `IN (...)` constraint.
   */
  literal<L extends string | number | boolean>(value: L): TypeBuilder<L, true> {
    if (value === null || value === undefined) {
      throw Object.assign(
        new Error("t.literal(value) requires a non-null primitive value"),
        { code: "literal_null_value" as const },
      );
    }
    const ty = typeof value;
    if (ty !== "string" && ty !== "number" && ty !== "boolean") {
      throw Object.assign(
        new Error(
          `t.literal(value): value must be string | number | boolean, got ${ty}`,
        ),
        { code: "literal_invalid_type" as const },
      );
    }
    // Literal values are inherently required — a literal field declares
    // "this row carries exactly this value", so an absent value would
    // make no sense. The `_required: true` brand surfaces in `InferSchema`
    // so the inferred type keeps the literal key non-optional.
    return new TypeBuilder<L, true>({ type: "literal", literalValue: value, required: true });
  },
  /**
   * C2 — discriminated union over object shapes. Each argument must be
   * a `t.object({...})` that declares at least one `t.literal(...)`
   * field; the SDK auto-detects the discriminator (the single key that
   * is a literal in every variant with mutually distinct values).
   *
   * ```ts
   * events: t.union(
   *   t.object({ kind: t.literal("login"), userId: t.ref("users"), ip: t.string() }),
   *   t.object({ kind: t.literal("error"), message: t.string() }),
   *   t.object({ kind: t.literal("metric"), name: t.string(), value: t.number() }),
   * )
   * ```
   *
   * **Storage** — flat columns (proposal §C2):
   * - One column per union-wide field (each nullable, since it only
   *   applies to a subset of variants). Fields that appear in multiple
   *   variants with the same type are deduplicated.
   * - The discriminator column carries `CHECK (kind IN (<all variant
   *   values>))`. Per-variant CHECK constraints enforce that variant-
   *   required fields are NOT NULL when the discriminator matches.
   *
   * **Validation** — dispatch on the discriminator value, run the
   * matching variant's schema. An unknown discriminator value fails
   * with a clear path-keyed error.
   *
   * **Type inference** — the union of each variant's inferred shape
   * (`InferSchema<V1> | InferSchema<V2> | ...`), so a value of the
   * inferred type narrows on the discriminator key:
   *
   * ```ts
   * const e: InferUnion<...> = ...;
   * if (e.kind === "login") {
   *   e.userId  // ✓ Id<"users">
   *   e.message // ✗ doesn't exist on the "login" variant
   * }
   * ```
   */
  union<V extends readonly TypeBuilder<any, any>[]>(...variants: V): TypeBuilder<InferUnion<V>> {
    if (variants.length < 2) {
      throw Object.assign(
        new Error(
          `t.union(...) requires at least 2 variants, got ${variants.length}`,
        ),
        { code: "union_too_few_variants" as const },
      );
    }
    const normalized: Record<string, FieldDef>[] = [];
    for (let i = 0; i < variants.length; i++) {
      const v = variants[i];
      if (!(v instanceof TypeBuilder)) {
        throw Object.assign(
          new Error(
            `t.union: variant #${i} must be a t.object(...) (got ${typeof v})`,
          ),
          { code: "union_variant_not_typebuilder" as const },
        );
      }
      const def = v.toFieldDef();
      if (def.type !== "object" || def.shape === undefined) {
        throw Object.assign(
          new Error(
            `t.union: variant #${i} must be a t.object(...) (got type "${def.type}")`,
          ),
          { code: "union_variant_not_object" as const },
        );
      }
      // Variant shape clone — we treat it as a self-contained sub-schema.
      const cloned: Record<string, FieldDef> = {};
      for (const [k, fd] of Object.entries(def.shape)) {
        cloned[k] = { ...fd };
      }
      normalized.push(cloned);
    }

    // Discriminator auto-detection. The discriminator is the unique
    // key that is `t.literal()` in EVERY variant AND has distinct
    // literal values across variants.
    const discriminator = detectDiscriminator(normalized);
    return new TypeBuilder<InferUnion<V>>({
      type: "union",
      variants: normalized,
      discriminator,
    });
  },
};

/**
 * Identify the discriminator field across a set of normalized union
 * variants. Returns the field name on success, throws an `Error` at
 * schema-definition time on failure so misconfigured unions never
 * reach validation.
 *
 * Algorithm: find every field name that appears as a `literal` in
 * every variant, then require exactly one such field whose literal
 * values are mutually distinct. If zero or more than one such field
 * exists, the union is ambiguous.
 */
function detectDiscriminator(variants: Record<string, FieldDef>[]): string {
  if (variants.length === 0) {
    throw Object.assign(
      new Error("t.union: no variants supplied"),
      { code: "union_no_variants" as const },
    );
  }
  // Candidate keys = keys that are `literal` in every variant.
  const firstKeys = Object.keys(variants[0]);
  const candidates: string[] = [];
  for (const key of firstKeys) {
    let ok = true;
    for (const v of variants) {
      const fd = v[key];
      if (fd === undefined || fd.type !== "literal") {
        ok = false;
        break;
      }
    }
    if (ok) candidates.push(key);
  }
  if (candidates.length === 0) {
    throw Object.assign(
      new Error(
        "t.union: no discriminator field found — every variant must declare a `t.literal(...)` field with the same key (e.g. `kind: t.literal(\"login\")`)",
      ),
      { code: "union_no_discriminator" as const },
    );
  }
  // For each candidate, the literal values must be mutually distinct.
  const distinctCandidates = candidates.filter((key) => {
    const seen = new Set<string>();
    for (const v of variants) {
      const lit = v[key]?.literalValue;
      const tag = typeof lit + ":" + String(lit);
      if (seen.has(tag)) return false;
      seen.add(tag);
    }
    return true;
  });
  if (distinctCandidates.length === 0) {
    throw Object.assign(
      new Error(
        "t.union: discriminator candidate(s) have overlapping literal values — each variant must use a distinct literal value",
      ),
      { code: "union_discriminator_overlap" as const },
    );
  }
  if (distinctCandidates.length > 1) {
    throw Object.assign(
      new Error(
        `t.union: ambiguous discriminator — multiple candidate keys with distinct literals: ${distinctCandidates.join(", ")}. Use only one literal field per variant or rename one of them.`,
      ),
      { code: "union_discriminator_ambiguous" as const },
    );
  }
  return distinctCandidates[0];
}

// ---------------------------------------------------------------------------
// C2 — Union type inference helpers
// ---------------------------------------------------------------------------

/**
 * Maps a tuple of variant TypeBuilders to the TS union of each
 * variant's inferred shape. Variants must each be `TypeBuilder<S>`
 * where `S = InferSchema<variantShape>` (the result type of
 * `t.object({...})`).
 *
 * The distributive `infer U` over a union of tuple elements gives us
 * the TS union of every variant's inferred value type.
 */
export type InferUnion<V extends readonly TypeBuilder<any, any>[]> =
  V[number] extends TypeBuilder<infer U, any> ? U : never;

// ---------------------------------------------------------------------------
// Schema builder — per-collection options via fluent API
// ---------------------------------------------------------------------------

/**
 * Per-collection strictness for deploy-time data validation
 * (proposal @zeroship/db, section A2). The default is `strict` to
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
 * Named multi-column index declaration produced by
 * `schema(...).index(name, fields)`. The SDK passes these through to
 * the native side alongside the schema; the orchestrator materialises
 * them as `CREATE INDEX CONCURRENTLY IF NOT EXISTS` statements and the
 * runtime warning path uses them to decide whether a filter is covered.
 *
 * `fields` order is significant — multi-column indexes only cover
 * filters whose keys form a prefix of the column list.
 */
export interface NamedIndexSpec {
  name: string;
  fields: string[];
  unique?: boolean;
}

/**
 * Wraps field definitions with per-collection options.
 * Use `schema({ ... }).softDelete()` to enable soft delete for a specific collection.
 */
export class SchemaBuilder<S> {
  readonly fields: S;
  private _options: SchemaOptions;
  private _indexes: NamedIndexSpec[];

  constructor(fields: S) {
    this.fields = fields;
    this._options = { softDelete: false, strictness: "strict", versioning: false };
    this._indexes = [];
  }

  /** Returns the collection options. */
  get options(): Readonly<SchemaOptions> { return this._options; }

  /** Returns the declared named indexes in declaration order. */
  get indexes(): readonly NamedIndexSpec[] { return this._indexes; }

  /**
   * Declare a named, multi-column index. Order matters — filters whose
   * keys form a prefix of `fields` are considered covered by the index.
   * The SDK passes the declaration to the native side, which materialises
   * a `CREATE INDEX CONCURRENTLY IF NOT EXISTS "<table>__<name>"` per
   * declared index. Auto-generated columns (`id`, `createdAt`, `updatedAt`,
   * `deletedAt`, `version`) are also accepted alongside user fields.
   *
   * Throws `Error` with `code = "schema_invalid"` at definition time if:
   *  - `name` is empty or already declared on this schema, or
   *  - `fields` is empty / contains a key absent from the schema.
   */
  index(name: string, fields: readonly string[]): this {
    this._addIndex(name, fields, false);
    return this;
  }

  /**
   * Same as {@link index} but materialises a `UNIQUE` index — enforces a
   * cross-column uniqueness constraint at the database layer. Useful for
   * compound natural keys (e.g. `["orgId", "slug"]`).
   */
  uniqueIndex(name: string, fields: readonly string[]): this {
    this._addIndex(name, fields, true);
    return this;
  }

  private _addIndex(name: string, fields: readonly string[], unique: boolean): void {
    if (typeof name !== "string" || name.length === 0) {
      throw Object.assign(
        new Error("schema.index(name, fields): name must be a non-empty string"),
        { code: "schema_invalid" },
      );
    }
    if (!Array.isArray(fields) || fields.length === 0) {
      throw Object.assign(
        new Error(`schema.index("${name}", fields): fields must be a non-empty array`),
        { code: "schema_invalid" },
      );
    }
    for (const existing of this._indexes) {
      if (existing.name === name) {
        throw Object.assign(
          new Error(`schema.index("${name}", ...): index name already declared on this schema`),
          { code: "schema_invalid" },
        );
      }
    }
    const known = this._knownFieldNames();
    for (const f of fields) {
      if (typeof f !== "string" || f.length === 0) {
        throw Object.assign(
          new Error(`schema.index("${name}", ...): every field must be a non-empty string`),
          { code: "schema_invalid" },
        );
      }
      if (!known.has(f)) {
        throw Object.assign(
          new Error(
            `schema.index("${name}", [..."${f}"...]): field "${f}" is not declared on this schema`,
          ),
          { code: "schema_invalid" },
        );
      }
    }
    const spec: NamedIndexSpec = { name, fields: [...fields] };
    if (unique) spec.unique = true;
    this._indexes.push(spec);
  }

  /**
   * The set of field names this schema accepts in `.index(...)`. Includes
   * user-declared fields plus the auto-generated columns the collection
   * always carries (`id`, `createdAt`, `updatedAt`); soft-delete /
   * versioning columns are accepted opportunistically when the matching
   * option is enabled so a `.softDelete().index("by_active", ["deletedAt"])`
   * declaration validates.
   */
  private _knownFieldNames(): Set<string> {
    const out = new Set<string>(["id", "createdAt", "updatedAt"]);
    if (this._options.softDelete) out.add("deletedAt");
    if (this._options.versioning) out.add("version");
    const f = this.fields;
    if (f !== null && typeof f === "object") {
      for (const k of Object.keys(f as Record<string, unknown>)) out.add(k);
    }
    return out;
  }

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
   * (A2 of the @zeroship/db proposal). Default is `strict`.
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
