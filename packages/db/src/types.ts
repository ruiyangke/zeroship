/**
 * `@zeroship/db` type surface.
 *
 * The schema-builder foundation - `TypeBuilder`, `FieldDef`, the `t.*` factories,
 * and the inference chain they are read by - now lives in `@zeroship/schema` and
 * is re-exported here, so every importer of `./types` and every consumer of
 * `@zeroship/db` keeps the same names and the same shapes.
 *
 * What remains is the part that belongs to the runtime SDK: transaction isolation,
 * the `Result` convention, the filter/sort/select/update algebra, the relation
 * surface, the naming strategy, and the per-collection schema-declaration surface.
 *
 * `IsolationLevel` stays here on purpose: it aliases the ambient
 * `ZeroshipIsolationLevel`, which the leaf must not depend on.
 */

import { TypeBuilder, t, decimal } from "@zeroship/schema";
import type {
  ColumnAssignment,
  PlainObject,
  JsonValue,
  Decimal,
  Id,
  IdValue,
  Row,
  RowId,
  RowInput,
  UpsertOptions,
  FkAction,
  FieldDefaultValue,
  RefOptions,
  FieldStorage,
  MaskKind,
  Classification,
  MaskOpts,
  MaskedValueRepr,
  MaskedValue,
  Actor,
  ArrayTypeDef,
  PrimitiveTypeName,
  DescriptorOnlyTypeName,
  TypeName,
  VectorMetric,
  FieldDef,
  InferUnion,
  InferFieldDef,
  InferSchema,
  InferInsertSchema,
  FilterKind,
  IsSchemaDict,
  AssignedKeys,
  RequiredKeys,
  DefaultKeys,
  ReadRequiredKeys,
  OptionalKeys,
  InsertRequiredKeys,
  InsertOptionalKeys,
  HasDefault,
} from "@zeroship/schema";

export { TypeBuilder, t, decimal };
export type {
  ColumnAssignment,
  PlainObject,
  JsonValue,
  Decimal,
  Id,
  IdValue,
  Row,
  RowId,
  RowInput,
  UpsertOptions,
  FkAction,
  FieldDefaultValue,
  RefOptions,
  FieldStorage,
  MaskKind,
  Classification,
  MaskOpts,
  MaskedValueRepr,
  MaskedValue,
  Actor,
  ArrayTypeDef,
  PrimitiveTypeName,
  DescriptorOnlyTypeName,
  TypeName,
  VectorMetric,
  FieldDef,
  InferUnion,
  InferFieldDef,
  InferSchema,
  InferInsertSchema,
  FilterKind,
  IsSchemaDict,
  AssignedKeys,
  RequiredKeys,
  DefaultKeys,
  ReadRequiredKeys,
  OptionalKeys,
  InsertRequiredKeys,
  InsertOptionalKeys,
  HasDefault,
};
/** PostgreSQL transaction isolation levels. */
/**
 * Postgres transaction isolation level. Alias of the ambient
 * `ZeroshipIsolationLevel` from `@zeroship/types/shared.d.ts` so the
 * `db.transaction({ isolationLevel })` option has one canonical spelling.
 */
export type IsolationLevel = ZeroshipIsolationLevel;

/** Return type for all Collection methods. Never throws — errors are values. */
export type Result<T> = { data: T; error: null } | { data: null; error: Error };

// ---------------------------------------------------------------------------
// Schema-to-TypeScript inference utilities
// ---------------------------------------------------------------------------

/**
 * Infers the value type from a field definition built via `t.*`.
 *
 * when the field is masked (third `TypeBuilder` brand
 * is a non-`"none"` mask kind), the inferred type wraps the bare
 * primitive in `MaskedValue<T>`. The hidden `__zs_raw__<col>` sibling
 * that holds the real value is NEVER part of `Row<S>` — only the
 * declared field appears, with the masked-value wrapper around it.
 */

// ---------------------------------------------------------------------------
// Filter types — typed query operators per field type
// ---------------------------------------------------------------------------

type EqualityOps<T> = {
  $eq?: T;
  $ne?: T | null;
  $in?: T[];
  $nin?: T[];
  $exists?: boolean;
};

type OrderingOps<T> = {
  $gt?: T;
  $gte?: T;
  $lt?: T;
  $lte?: T;
};

type InferredFilterKind<T> = NonNullable<T> extends Decimal
  ? "exact"
  : NonNullable<T> extends string
  ? "text"
  : NonNullable<T> extends number | bigint
    ? "ordered"
    : "equality";

/** String-specific operators. */
type StringOps = {
  $like?: string;
  $ilike?: string;
};

/**
 * Filter value for an ordinary field — either a direct value, null, or
 * operator object.
 *
 * `$like` / `$ilike` are real backend operators: the ORM query builder
 * validates them and lowers them to SQL predicates.
 * Keep kind checks non-distributive to bound generic operator intersections.
 */
type PlainFilterValue<T, K extends FilterKind = InferredFilterKind<T>> =
  [K] extends ["search"]
    ? never
    : ([K] extends ["json"] ? null : T | null) | (
      EqualityOps<NonNullable<T>> &
      ([K] extends ["text"]
        ? OrderingOps<NonNullable<T>> & StringOps
        : [K] extends ["ordered"]
          ? OrderingOps<NonNullable<T>>
          : object)
    );


/**
 * Filter legality derived from the schema field builder itself.
 *
 * This keeps the type layer aligned with the runtime fence in
 * `validateEncryptedFieldsInFilter`:
 * - encrypted fields cannot be filtered
 * - plain fields keep the normal operator surface
 */
type FilterValueForFieldBuilder<F> =
  F extends {
    readonly _type: unknown;
    readonly _encryption: true | undefined;
    readonly _filterKind: FilterKind;
  }
    ? true extends F["_encryption"]
      ? never
      : PlainFilterValue<NonNullable<F["_type"]>, F["_filterKind"]>
    : never;

type FieldFilters<S> = IsSchemaDict<S> extends true
  ? { [K in keyof S]?: FilterValueForFieldBuilder<NonNullable<S[K]>> }
  : { [K in keyof Row<S>]?: PlainFilterValue<NonNullable<Row<S>[K]>> };

/** Typed filter for a document — each field accepts its value type or operators.
 *  Field values use `NonNullable<Row<S>[K]>` so `undefined` is rejected at the
 *  type layer; pass `null` to match SQL NULL explicitly. */
export type Filter<S> = FieldFilters<S> & {
  $and?: Filter<S>[];
  $or?: Filter<S>[];
  $not?: Filter<S>;
};

type SortableBuilder<F> = F extends {
  readonly _encryption: infer E;
  readonly _mask: infer M;
  readonly _filterKind: infer K;
}
  ? true extends E
    ? false
    : M extends Exclude<MaskKind, "none">
      ? true
      : K extends "text" | "ordered"
        ? true
        : false
  : false;

type DistinctBuilder<F> = F extends {
  readonly _encryption: infer E;
  readonly _filterKind: infer K;
}
  ? true extends E
    ? false
    : K extends "json" | "search" | "exact"
      ? false
      : true
  : false;

/** Fields with the same order on every supported database. */
export type SortableField<S> = string & (IsSchemaDict<S> extends true
  ? {
      [K in keyof S]-?: SortableBuilder<NonNullable<S[K]>> extends true ? K : never;
    }[keyof S]
  : string extends keyof Row<S>
    ? string
    : {
        [K in keyof Row<S>]-?: NonNullable<Row<S>[K]> extends
          | string
          | number
          | bigint
          | MaskedValue<string | number | bigint | Uint8Array>
          ? K
          : never;
      }[keyof Row<S>] | "id");

/** Fields with portable database equality for grouping and deduplication. */
export type DistinctField<S> = string & (IsSchemaDict<S> extends true
  ? {
      [K in keyof S]-?: DistinctBuilder<NonNullable<S[K]>> extends true ? K : never;
    }[keyof S]
  : string extends keyof Row<S>
    ? string
    : {
        [K in keyof Row<S>]-?: NonNullable<Row<S>[K]> extends
          | string
          | number
          | bigint
          | boolean
          | Uint8Array
          | MaskedValue<string | number | bigint | Uint8Array>
          ? K
          : never;
      }[keyof Row<S>] | "id");

type SearchField<S, Shape> = string & (IsSchemaDict<S> extends true
  ? {
      [K in keyof S]-?: NonNullable<S[K]> extends {
        readonly _filterKind: "search";
        readonly _type: infer T;
      }
        ? T extends Shape ? K : never
        : never;
    }[keyof S]
  : string extends keyof Row<S>
    ? string
    : {
        [K in keyof Row<S>]-?: NonNullable<Row<S>[K]> extends Shape ? K : never;
      }[keyof Row<S>]);

/** Fields accepted as vector-search inputs. */
export type VectorField<S> = SearchField<S, readonly number[]>;
/** Fields accepted as within-radius spatial-search inputs. */
export type GeoField<S> = SearchField<S, { lat: number; lng: number }>;

type AtLeastOne<T> = {
  [K in keyof T]-?: Required<Pick<T, K>> & Partial<Omit<T, K>>;
}[keyof T];

export type SortSpec<S> = [SortableField<S>] extends [never]
  ? never
  : AtLeastOne<Record<SortableField<S>, 1 | -1>>;
export type SortInput<S> = SortSpec<S> | SortableField<S> | `-${SortableField<S>}`;

/** Fields exposed by a typed projection. */
export type SelectableField<S> = string & keyof Row<S>;
export type SelectSpec<S> = string extends SelectableField<S>
  ? Record<string, number | boolean>
  : [SelectableField<S>] extends [never]
    ? never
    : AtLeastOne<Record<SelectableField<S>, 1 | true>>;
export type SelectInput<S> = string extends SelectableField<S>
  ? string | readonly string[] | Record<string, number | boolean>
  : SelectableField<S> | readonly SelectableField<S>[] | SelectSpec<S>;

// ---------------------------------------------------------------------------
// Update expression types — typed operators per field type
// ---------------------------------------------------------------------------

/** Numeric update operators. */
type NumericUpdateOps<T extends number | bigint | Decimal> = {
  $inc?: T;
  $dec?: T;
  $mul?: T;
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
  (NonNullable<T> extends number | bigint | Decimal ? NumericUpdateOps<NonNullable<T>> : never) |
  (NonNullable<T> extends readonly unknown[] ? ArrayUpdateOps<NonNullable<T>[number]> : never);

type UpdateKeys<S> = Exclude<keyof InferSchema<S>, AssignedKeys<S> | "id">;

/** Typed update expression — per-field operators. */
export type UpdateExpression<S> = {
  [K in UpdateKeys<S>]?: UpdateFieldValue<InferSchema<S>[K]>
} & {
  // Document operators share the ORM's assignment grammar.
  $set?: Partial<Pick<InferSchema<S>, UpdateKeys<S>>>;
  $inc?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends number | bigint | Decimal ? NonNullable<InferSchema<S>[K]> : never };
  $dec?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends number | bigint | Decimal ? NonNullable<InferSchema<S>[K]> : never };
  $mul?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends number | bigint | Decimal ? NonNullable<InferSchema<S>[K]> : never };
  $push?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
  $pull?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
  $addToSet?: { [K in UpdateKeys<S>]?: NonNullable<InferSchema<S>[K]> extends readonly unknown[] ? NonNullable<InferSchema<S>[K]>[number] : never };
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
// InferRow / InferRowInput / InferId — type-only helpers that pull `Row` /
// `RowInput` / `Id` out of a
// Collection without `Parameters<typeof col.insertMany>[0][number]` plumbing.
// ---------------------------------------------------------------------------

/** Persisted-row type for a Collection — `Row<S>` for `Collection<S, _>`. */
export type InferRow<C> =
  C extends { readonly _schema_brand?: infer S } ? Row<S> :
  C extends import("./db-types").Collection<infer S, any, any> ? Row<S> :
  never;

/** Insert-shape type for a Collection — `RowInput<S>` for `Collection<S, _>`. */
export type InferRowInput<C> =
  C extends { readonly _schema_brand?: infer S } ? RowInput<S> :
  C extends import("./db-types").Collection<infer S, any, any> ? RowInput<S> :
  never;

/** Branded `Id<N>` for a Collection — `Id<"users">` for `Collection<_, "users">`. */
export type InferId<C> =
  C extends import("./db-types").Collection<infer S, infer N extends string, any> ? Id<N, RowId<S>> :
  never;

/** Declared foreign-key fields available for relation loading. */
export type RelationField<S> = string & (IsSchemaDict<S> extends true
  ? {
      [K in keyof S]-?: [ExtractRefTarget<NonNullable<S[K]>>] extends [never] ? never : K;
    }[keyof S]
  : string extends keyof Row<S>
    ? string
    : {
        [K in keyof Row<S>]-?: [ExtractRefTarget<NonNullable<Row<S>[K]>>] extends [never]
          ? never
          : K;
      }[keyof Row<S>]);

type DeclaredRelation<X> = X extends { readonly _relation: infer N extends string }
  ? N : X extends { relation: infer N extends string } ? N : never;

/** Named schema edges, distinct from their scalar foreign-key fields. */
export type RelationName<S> = string & (string extends keyof Row<S> ? string : {
  [K in keyof S]: DeclaredRelation<NonNullable<S[K]>>;
}[keyof S]);

export type WithSpec<S = PlainObject> = Partial<Record<RelationName<S>, true>>;

export type ExactWithSpec<S, W extends WithSpec<S>> = keyof W extends never ? never : W & Record<keyof W, true> &
  Record<Exclude<keyof W, RelationName<S>> | Extract<keyof W, `_${string}` | "constructor" | "prototype">, never>;

/** Resolve reference targets from builder identity metadata or field descriptors. */
export type ExtractRefTarget<X> =
  X extends TypeBuilder<infer U, any, any, any, any>
    ? U extends { readonly __zeroshipTable: infer T extends string }
      ? T
      : never
    : X extends { readonly __zeroshipTable: infer T extends string }
      ? T
      : X extends { refTarget: infer T extends string }
        ? T
        : never;

/**
 * Unwrap whatever shape an `AllSchemas[name]` slot holds into the raw
 * field-record the `Row<...>` machinery understands. Mirrors the
 * `UnwrapSchema<T>` alias in `db-types.ts` but lives here so `WithRelations`
 * can call it without importing across the module boundary.
 *
 * - `schema({...})` wraps `Record<string, TypeBuilder>` — strip it.
 * - A top-level `t.union(...)` yields `TypeBuilder<UnionShape>` — strip
 *   to `UnionShape` so the discriminator narrows correctly.
 * - Plain field-record passes through unchanged.
 */
export type UnwrapSchemaForRelation<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any, any, any, any> ? U :
  T;

/**
 * Resolve the target table's `Row<...>` given the field type at `S[K]`
 * and the parent database's schema map. Standalone models without that map
 * use `PlainObject`; installed schemas resolve the declared target row.
 */
export type ResolveTargetRow<X, AllSchemas> =
  ExtractRefTarget<X> extends infer Target
    ? Target extends keyof AllSchemas
      ? Row<UnwrapSchemaForRelation<AllSchemas[Target]>>
      : Target extends string
        ? PlainObject
        : never
    : never;

/** Attach target row types to requested named edges. */
export type WithRelations<S, W extends WithSpec<S>, AllSchemas = Record<string, unknown>> = {
  [N in keyof W]: string extends keyof Row<S> ? PlainObject | null : {
    [K in keyof S]: N extends DeclaredRelation<NonNullable<S[K]>>
      ? ResolveTargetRow<S[K], AllSchemas> | null : never;
  }[keyof S];
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
const SCHEMA_BUILDER_BRAND = Symbol.for("@zeroship/db/SchemaBuilder");


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
 *   `schemaValidation: false`); intended for externally managed data.
 */
export type Strictness = "strict" | "lenient" | "off";

/** Options that can be set per-collection via the schema() builder. */
export interface SchemaOptions {
  softDelete: boolean;
  strictness: Strictness;
  /**
   * Enables the migration generator that emits a descriptor-declared
   * concurrency column. Runtime compare-and-swap behavior follows the
   * field carrying `concurrency: true`, including when that field is renamed.
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
  readonly [SCHEMA_BUILDER_BRAND] = true;

  readonly fields: S;
  private _options: SchemaOptions;
  private _indexes: NamedIndexSpec[];

  static [Symbol.hasInstance](value: unknown): boolean {
    return Boolean(
      value &&
        typeof value === "object" &&
        (value as Record<PropertyKey, unknown>)[SCHEMA_BUILDER_BRAND] === true &&
        "fields" in value,
    );
  }

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
   * Declare an ordered index over fields present in this schema.
   *
   * Throws `Error` with `code = "SCHEMA_INVALID"` at definition time if:
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
        { code: "SCHEMA_INVALID" },
      );
    }
    if (!Array.isArray(fields) || fields.length === 0) {
      throw Object.assign(
        new Error(`schema.index("${name}", fields): fields must be a non-empty array`),
        { code: "SCHEMA_INVALID" },
      );
    }
    for (const existing of this._indexes) {
      if (existing.name === name) {
        throw Object.assign(
          new Error(`schema.index("${name}", ...): index name already declared on this schema`),
          { code: "SCHEMA_INVALID" },
        );
      }
    }
    const known = this._knownFieldNames();
    for (const f of fields) {
      if (typeof f !== "string" || f.length === 0) {
        throw Object.assign(
          new Error(`schema.index("${name}", ...): every field must be a non-empty string`),
          { code: "SCHEMA_INVALID" },
        );
      }
      if (!known.has(f)) {
        throw Object.assign(
          new Error(
            `schema.index("${name}", [..."${f}"...]): field "${f}" is not declared on this schema`,
          ),
          { code: "SCHEMA_INVALID" },
        );
      }
    }
    const spec: NamedIndexSpec = { name, fields: [...fields] };
    if (unique) spec.unique = true;
    this._indexes.push(spec);
  }

  private _knownFieldNames(): Set<string> {
    return new Set(Object.keys(this.fields ?? {}));
  }

  /** Enable the descriptor's soft-delete lifecycle. */
  softDelete(): this {
    this._options.softDelete = true;
    return this;
  }

  /** Enable compare-and-swap using the declared concurrency field. */
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
