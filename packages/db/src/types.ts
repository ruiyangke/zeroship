/**
 * Core type definitions and the `t` type-builder API for @zeroship/db.
 * Use `t.string()`, `t.number()`, etc. to declare schema fields with
 * optional constraints, then export the schema map via
 * `export default { schema: { ... } }`.
 */

export interface ColumnAssignment {
  readonly by: "now" | "typedId" | "actor" | "identity" | `increment(${number})`;
  readonly on: "insert" | "write" | "delete";
}

/** Generic plain object type used throughout the SDK. */
export type PlainObject = Record<string, unknown>;

/** A JSON column can hold an object, array, or scalar at its root. */
export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

declare const decimalBrand: unique symbol;
export type Decimal = string & { readonly [decimalBrand]: "Decimal" };

const DECIMAL_PATTERN = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/;
const MAX_DECIMAL_INPUT_DIGITS = 4096;
const MAX_DECIMAL_EXPONENT = 4096;

export function decimal(value: string): Decimal {
  if (value.length > MAX_DECIMAL_INPUT_DIGITS) {
    throw new TypeError("decimal value must be a bounded JSON decimal string");
  }
  const exponent = /[eE]([+-]?\d+)$/.exec(value)?.[1];
  const fraction = /\.(\d+)/.exec(value)?.[1] ?? "";
  const digits = value.replace(/^-/, "").split(/[eE]/, 1)[0].replace(".", "");
  const exponentValue = exponent === undefined ? 0 : Number(exponent);
  const scale = fraction.length - exponentValue;
  if (
    !DECIMAL_PATTERN.test(value) ||
    digits.length > MAX_DECIMAL_INPUT_DIGITS ||
    !Number.isSafeInteger(exponentValue) ||
    Math.abs(exponentValue) > MAX_DECIMAL_EXPONENT ||
    scale > MAX_DECIMAL_INPUT_DIGITS ||
    (scale < 0 && digits.length - scale > MAX_DECIMAL_INPUT_DIGITS)
  ) {
    throw new TypeError("decimal value must be a bounded JSON decimal string");
  }
  return value as Decimal;
}

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
export type InferFieldDef<T> =
  T extends TypeBuilder<infer U, any, infer M, any, any, any>
    ? M extends MaskKind
      ? M extends "none"
        ? U
        : U extends string | number | bigint | Uint8Array
          ? MaskedValue<U>
          : U
      : U
    : unknown;

/** Keys whose field builder was marked `.required()` (the `R` brand of
 *  `TypeBuilder<_, R>`). */
export type RequiredKeys<S> = {
  [K in keyof S]:
    S[K] extends TypeBuilder<any, true, any, any, any> ? K :
    never
}[keyof S];

/** Keys whose field builder has a platform-applied insert default. */
export type DefaultKeys<S> = {
  [K in keyof S]:
    S[K] extends TypeBuilder<any, any, any, any, true> ? K :
    never
}[keyof S];

/** Read rows include required fields and fields populated by defaults. */
export type ReadRequiredKeys<S> = RequiredKeys<S> | DefaultKeys<S>;

/** Keys that are not explicitly required. */
export type OptionalKeys<S> = Exclude<keyof S, ReadRequiredKeys<S>>;

type HasDefault<T> =
  T extends TypeBuilder<any, any, any, any, true> ? true :
  false;

/** Required insert keys exclude fields with a schema-level `.default()`. */
export type InsertRequiredKeys<S> = {
  [K in keyof S]:
    S[K] extends TypeBuilder<any, true, any, any, any>
      ? HasDefault<S[K]> extends true ? never : K
      : never
}[keyof S];

/** Insert-optional keys include ordinary optional fields and defaulted required fields. */
export type InsertOptionalKeys<S> = Exclude<keyof S, InsertRequiredKeys<S>>;

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
        [K in keyof S]-?: NonNullable<S[K]> extends {
          readonly _type: unknown;
          toFieldDef(): Readonly<FieldDef>;
        } ? true : false;
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
        [K in ReadRequiredKeys<T>]: InferFieldDef<T[K]>;
      } & {
        [K in OptionalKeys<T>]?: InferFieldDef<T[K]>;
      }
    : T
  : never;

/** Insert-time shape inference. Required + defaulted fields become optional. */
export type InferInsertSchema<S> = S extends infer T
  ? IsSchemaDict<T> extends true
    ? {
        [K in Exclude<InsertRequiredKeys<T>, AssignedKeys<T>>]: InferFieldDef<T[K]>;
      } & {
        [K in Exclude<InsertOptionalKeys<T>, AssignedKeys<T>>]?: InferFieldDef<T[K]>;
      }
    : T
  : never;

/** The persisted fields declared by the collection schema. */
export type Row<S> = InferSchema<S>;

export type IdValue = string | number | bigint;
/** The identity type declared by the collection schema. */
export type RowId<S> = "id" extends keyof Row<S>
  ? unknown extends Row<S>["id"] ? IdValue : Extract<Row<S>["id"], IdValue>
  : IdValue;

export type AssignedKeys<S> = {
  [K in keyof S]: S[K] extends { readonly _assigned: true } ? K : never
}[keyof S];

/** Generated fields are read-only inputs. */
export type RowInput<S> = InferInsertSchema<S> & { [K in AssignedKeys<S>]?: never };

export type UpsertOptions<S> = {
  conflictFields: Exclude<string & keyof Row<S>, AssignedKeys<S>>[];
};

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

type FilterKind = "text" | "ordered" | "equality" | "exact" | "json" | "search";
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
  C extends import("./collection").Collection<infer S, any, any> ? Row<S> :
  never;

/** Insert-shape type for a Collection — `RowInput<S>` for `Collection<S, _>`. */
export type InferRowInput<C> =
  C extends { readonly _schema_brand?: infer S } ? RowInput<S> :
  C extends import("./collection").Collection<infer S, any, any> ? RowInput<S> :
  never;

/** Branded `Id<N>` for a Collection — `Id<"users">` for `Collection<_, "users">`. */
export type InferId<C> =
  C extends import("./collection").Collection<infer S, infer N extends string, any> ? Id<N, RowId<S>> :
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
export type PrimitiveTypeName = "string" | "number" | "boolean" | "date" | "json" | "calendarDate";


/**
 * built-in mask transform applied at write time to compute the value
 * stored in the field's own column from the plaintext (which is
 * relocated to `__zs_raw__<col>` by the same write). Mirrors `crate::diff::MaskKind` on the Rust side.
 *
 * - `full`        — `"***"`; maximum redaction. Default for encrypted.
 * - `last4`       — `"***-**-6789"`; SSN / card / phone tails.
 * - `first4`      — `"4111-****-****-****"`; BIN / IIN preservation.
 * - `email`       — `"a****@example.com"`; preserve domain.
 * - `name`        — `"A. A***"`; initials.
 * - `date-year`   — `"1985-**-**"`; preserve year for age buckets.
 * - `date-decade` — `"198?-**-**"`; preserve decade.
 * - `none`        — explicit opt-out: no sibling, no mask wrap on
 *                   read. Reserved for encrypted columns the creator
 *                   wants plaintext-on-read for (background jobs
 *                   that operate at a trust boundary).
 *
 * No raw user-defined JS functions for masking — they would let an
 * AI-generated `mask: v => v` defeat the purpose. Adding a new
 * mask kind requires a platform release.
 */
export type MaskKind =
  | "full"
  | "last4"
  | "first4"
  | "email"
  | "name"
  | "date-year"
  | "date-decade"
  | "none";

/**
 * Sensitivity classification used for unmask authorization and auditing.
 *
 * - `public`   — usernames, display names, public profile data.
 * - `pii`      — full name, email, address, phone, IP, date of birth.
 *                Default classification for encrypted columns without
 *                explicit `.mask(...)`.
 * - `spi`      — SSN, driver's license, biometric data (CPRA
 *                "sensitive PI").
 * - `phi`      — health records, medical IDs, diagnosis (HIPAA scope).
 * - `pci`      — card numbers, CVV, magnetic stripe (PCI-DSS scope).
 * - `internal` — application-internal metadata.
 *
 * These names are reserved as columns by the ORM identifier policy so creator
 * schemas cannot collide with the taxonomy.
 */
export type Classification =
  | "public"
  | "pii"
  | "spi"
  | "phi"
  | "pci"
  | "internal";

/**
 * options accepted by `.mask(opts)` on a `TypeBuilder`.
 *
 * - `kind`            — required. The mask transform; see {@link MaskKind}.
 * - `classification`  — optional. Defaults to `"pii"` when omitted.
 */
export interface MaskOpts {
  kind: MaskKind;
  classification?: Classification;
}

/**
 * wire shape the Rust read path emits for masked
 * columns (the `__zsmask__` sentinel object).
 *
 * this wire shape is now consumed entirely Rust-side:
 * the row serializer emits the sentinel into the JSON string, and the
 * runtime's rehydration pass (`masked_value::rehydrate_masked_values`)
 * replaces it with a native {@link MaskedValue} v8_class instance at
 * `JSON.parse` time. The SDK never observes the raw sentinel and never
 * constructs `MaskedValue`; this type is retained as documentation of
 * the wire contract (and is still used by the test harness to
 * synthesise sentinel payloads).
 *
 * Wire shape:
 * ```json
 * {
 *   "sentinel": "__zsmask__",
 *   "masked": "***-**-6789",
 *   "classification": "spi",
 *   "_meta": { "collection": "users", "row_pk": "usr_…", "column": "ssn" }
 * }
 * ```
 */
export interface MaskedValueRepr {
  /** The masked representation. Safe to log, serialize, render. */
  masked: string;
  /** Classification of the source field — see {@link Classification}. */
  classification: Classification;
  /** Wire-shape sentinel. Always the string literal `"__zsmask__"`. */
  sentinel: "__zsmask__";
}

/** Actor descriptor passed to unmask authorization. */
export type Actor = Record<string, unknown>;

/**
 * masked-value wrapper, now a NATIVE v8_class.
 *
 * `MaskedValue` instances are minted Rust-side by the row serializer's
 * rehydration pass (`crates/zeroship-data-v8/src/v8_classes/masked_value.rs`)
 * when a masked column flows back across the V8 boundary. The SDK no
 * longer constructs them — this is a TYPE-ONLY `declare class` that
 * describes the native instance's shape. There is no JS runtime body;
 * `new MaskedValue(...)` from user code throws `Illegal constructor`
 * (the native constructor rejects).
 *
 * Encapsulates the masked representation of a sensitive field along
 * with its classification + per-row `_meta`. Reads of a masked column
 * return `MaskedValue<T>` instead of a bare `T`; the only paths to
 * plaintext are:
 *
 * 1. `.unmask({ reason?, actor? })` — native round-trip: authorization
 *    check, audit-row emission, decrypt, returns the bare plaintext.
 * 2. `.unmask(columns, opts)` — native multi-column fan-out pinned to
 *    this row; resolves with `Record<col, plaintext>`. Atomic auth.
 * 3. The per-query unmask hint
 *    `db.users.find({ id }, { unmask: ["ssn"], actor }).first()` — the
 *    listed columns come back as bare plaintext, not wrapped.
 *
 * Coercion: `.toString()` / `JSON.stringify()` / template-literal
 * interpolation all yield the masked string, so `console.log(user)`
 * never leaks plaintext.
 */
export declare class MaskedValue<T extends string | number | bigint | Uint8Array = string> {
  /** @internal Phantom for the plaintext type. Erased at runtime. */
  readonly _plaintext: T;

  /** The masked representation. Safe to log, serialize, render. */
  readonly masked: string;

  /** Classification of the source field — see {@link Classification}. */
  readonly classification: Classification;

  /** @internal Frozen per-row coordinates the native `unmask` round-trip
   *  uses. Re-nested from the native instance's flat internal fields. */
  readonly _meta: Readonly<{ collection: string; row_pk: string; column: string }>;

  /**
   * Round-trip to the platform to fetch plaintext.
   *
   * The native dispatcher (bound to this instance's `_meta`):
   * 1. Looks up the column's mask + encryption metadata.
   * 2. Authorises the actor against the column's classification.
   * 3. SELECTs the row, decrypts (encrypted) or reads the parent
   *    column directly (mask-only).
   * 4. Writes one row to `<app>.__zeroship_audit_unmask` (granted AND
   *    denied paths both audit, per Q-MASK-C).
   * 5. Resolves with the bare plaintext on success; rejects with a
   *    coded error otherwise.
   *
   * Errors: `unmask_not_permitted`, `unmask_column_not_masked`,
   * `unmask_not_found`, `unmask_value_null`.
   *
   * The `columns` overload fans out to a native bulk unmask pinned to
   * this MaskedValue's row, returning `Record<col, T>`. Atomic
   * authorization: one denied column rejects the whole fan-out with
   * `BULK_UNMASK_PARTIAL_UNAUTHORIZED`.
   *
   * Binary plaintext arrives as Uint8Array; numeric plaintext arrives as a number.
   */
  unmask(opts?: { actor?: Actor; reason?: string }): Promise<T>;
  unmask(
    columns: readonly string[],
    opts: { actor: Actor; reason?: string },
  ): Promise<Record<string, T>>;

  /**
   * Check whether `actor` (or the current request's actor) is
   * authorized to unmask this field. Implemented native-side as a
   * dry-run unmask: issues a real unmask with reason
   * `"permission probe"`, treats `unmask_not_permitted` as `false`, and
   * rethrows every other error. Per Q-MASK-C the probe DOES write an
   * audit row.
   */
  canUnmask(opts?: { actor?: Actor }): Promise<boolean>;

  /** Implicit string coercion → masked representation. */
  toString(): string;

  /** JSON serialisation → masked representation. */
  toJSON(): string;
}

/**
 * options accepted by `t.encrypted(opts?)`.
 *
 * The host supplies the project encryption key; schemas contain no key selector.
 */
export interface EncryptedFieldOpts {
  /**
   * Inner type the encrypted value wraps. Only string / number / bytes
   * are supported. Passing any other `TypeBuilder` throws with code
   * `ENCRYPTED_TYPE_UNSUPPORTED` at schema-definition time.
   */
  of?: TypeBuilder<any, any, any, any, any>;
}
/** Definition for an array field with a declared item type. */
export type ArrayTypeDef = { type: "array"; items: PrimitiveTypeName };
/**
 * Column names that reach the runtime descriptor but that nobody authors.
 *
 * `PrimitiveTypeName` is the surface a creator writes. These are what the
 * generator emits when it reproduces the columns the MIGRATIONS built:
 * `t.int()` becomes `"int"` in `schema.runtime.json`, not `"number"`. They
 * therefore arrive at `validateValue`, and leaving them out of the union does
 * not keep them out of the data -- it only stops the code that handles them
 * from typechecking. `render-env-db.ts` switches on exactly these names.
 *
 * That is not hypothetical. The integer handling and the fail-closed unknown
 * type guard in `validate.ts` were both added after an `int` field was
 * measured accepting the string `"abc"` while the same field declared
 * `"number"` rejected it. The union was never widened to match, so those
 * comparisons became `TS2367` "no overlap" errors and `pnpm build` failed
 * workspace-wide -- with the runtime behaviour correct the whole time.
 *
 * `timestamp` is here on the generator's authority rather than on a sighting:
 * no descriptor in this repo currently emits it, because no example calls
 * `t.timestamp()`, but the renderer handles it beside `date`. Waiting for an
 * example to use it is how the integer case stayed broken.
 */
export type DescriptorOnlyTypeName = "int" | "integer" | "bigInt" | "float" | "timestamp";

export type TypeName = PrimitiveTypeName | DescriptorOnlyTypeName | "array" | "ref" | "object" | "literal" | "union" | "vector" | "geoPoint" | "bytes" | "id";

/**
 * distance metric for `t.vector(...)` fields. The three
 * metrics map 1:1 to pgvector operator classes (`vector_cosine_ops`,
 * `vector_l2_ops`, `vector_ip_ops`) and to the matching `VectorMetric`
 * variants on the Rust side.
 *
 * - `cosine` — Cosine distance. Default for L2-normalised embeddings.
 * - `l2` — Euclidean (L2) distance.
 * - `innerProduct` — Negative inner product (negated so smaller-is-better
 *   holds across all three metrics).
 */
export type VectorMetric = "cosine" | "l2" | "innerProduct";

/** Union of all values that can serve as a field default. */
export type FieldDefaultValue = string | number | boolean | Date | Uint8Array | null | PlainObject | string[] | number[] | boolean[];

/**
 * Foreign-key action policy for `t.ref()` (proposal B2).
 *
 * - `restrict`  — refuse to delete/update the parent row immediately if any
 *                 child references it.
 * - `cascade`   — child rows are deleted/updated along with the parent.
 * - `set null`  — child reference column is nulled when parent is deleted.
 *                 Only valid when the column is nullable.
 * - `no action` — SQL/Postgres default; checks at statement end, or at
 *                 constraint time when paired with an explicit deferrable FK.
 */
export type FkAction = "restrict" | "cascade" | "set null" | "no action";

/**
 * Options accepted by `t.ref()` to control FK behaviour at the DB layer.
 */
export interface RefOptions {
  /** Logical edge name used by with(). */
  relation?: string;
  /** Target column. Required when authoring a foreign key through a manual schema. */
  column?: string;
  /** ON DELETE policy. Omitted means SQL/Postgres `NO ACTION`. */
  onDelete?: FkAction;
  /** ON UPDATE policy. Omitted means SQL/Postgres `NO ACTION`. */
  onUpdate?: FkAction;
  /**
   * Emit `DEFERRABLE INITIALLY DEFERRED` for this FK so the constraint
   * check is queued until COMMIT (lets circular refs be inserted in any
   * order within one tx). Omitted means the SQL/Postgres default:
   * `NOT DEFERRABLE`.
   */
  deferrable?: boolean;
}

/** Collection identity brand; the underlying value follows the declared ID type. */
export type Id<T extends string, V extends IdValue = string> = V & {
  readonly __zeroshipTable: T;
};

/**
 * **Where one declared field physically lives** (runtime descriptor v2).
 *
 * A declared field is not always one column. A masked field occupies two: the value a
 * default projection reads (the field's OWN column, holding the mask), and the
 * authoritative value behind it (`__zs_raw__<col>`). Recording both is what lets a
 * consumer stop deriving the second name by string formatting.
 *
 * **The AEAD binds the LOGICAL FIELD NAME, not the physical column.** `canonical_aad`
 * receives the schema field key, in the encryption pass and the unmask path alike. The
 * two were the same string before the storage flip, which is why the distinction was
 * invisible; they are not now, and the rule that survived is the logical one. That
 * makes the flip a rename rather than a re-encrypt.
 *
 * **Why there is no separate `aadColumn`.** `canonical_aad` must use the logical
 * field name: every stored cell is authenticated under it, so binding `rawColumn`
 * instead would destroy every ciphertext in the deployment and make moving an
 * encrypted value a re-encrypt. A field that can disagree with the rule is a second
 * source of truth for one fact.
 *
 * Emitted by the migration fold, which is the only producer that knows physical
 * layout. It is absent on a `FieldDef` built by the `t.*()` authoring builders, which
 * describe a field's TYPE and know nothing about where it lands.
 */
export interface FieldStorage {
  /** The column a default projection reads under the field's logical name. */
  valueColumn: string;
  /**
   * The column holding the authoritative value - plaintext for a mask-only field,
   * ciphertext for an encrypted one - when that is a different physical object from
   * `valueColumn`. Absent when the field occupies exactly one column.
   */
  rawColumn?: string;
  /**
   * May a creator-facing filter reach `rawColumn`? Present only alongside one.
   * Declared, never inferred - and in particular never inferred from a name suffix.
   */
  rawFilterable?: boolean;
  /** May a creator-facing `orderBy` reach `rawColumn`? See `rawFilterable`. */
  rawSortable?: boolean;
  /** May a creator-facing projection return `rawColumn`? See `rawFilterable`. */
  rawProjectable?: boolean;
}

/** Internal representation of a fully-specified field definition used by validate and collection. */
export interface FieldDef {
  type: TypeName;
  items?: PrimitiveTypeName;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: FieldDefaultValue | (() => FieldDefaultValue);
  /** Generator and lifecycle event supplied by the runtime descriptor. */
  assign?: ColumnAssignment;
  primaryKey?: boolean;
  softDelete?: boolean;
  concurrency?: boolean;
  writable?: boolean;
  min?: number;
  max?: number;
  precision?: number;
  scale?: number;
  enum?: (string | number)[];
  pattern?: RegExp;
  /** Referenced table; independent of the field's storage type. */
  refTarget?: string;
  refColumn?: string;
  /** Logical edge name used by relation loading. */
  relation?: string;
  /** ON DELETE policy for `t.ref()`. Omitted means SQL/Postgres `NO ACTION`. */
  onDelete?: FkAction;
  /** ON UPDATE policy for `t.ref()`. Omitted means SQL/Postgres `NO ACTION`. */
  onUpdate?: FkAction;
  /**
   * Whether the FK is emitted DEFERRABLE INITIALLY DEFERRED. Omitted
   * means SQL/Postgres `NOT DEFERRABLE`.
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
  /**
   * declared dimensionality of a `t.vector(...)` field.
   * Present iff `type === "vector"`. The DDL emitter (PG arm) renders
   * `vector(N)` with this value; the index emitter routes through
   * `VectorIndex::ensure_vector_index`. Range: `1..=16000` (pgvector
   * hard ceiling).
   */
  vectorDims?: number;
  /**
   * distance metric for a `t.vector(...)` field. Present
   * iff `type === "vector"`. Selects the pgvector opclass for the
   * ivfflat index and the operator for ORDER BY at search time.
   */
  vectorMetric?: VectorMetric;
  /** Whether the field uses encrypted storage; `type` describes its plaintext. */
  encrypted?: boolean;
  /**
   * column-mask metadata. Present iff the SDK declared
   * the column with `.mask({ kind, classification? })`, OR the column
   * is `t.encrypted(...)` without explicit `.mask(...)` and the
   * schema-normaliser auto-populates the default mask
   * (`{ kind: "full", classification: "pii" }`).
   *
   * When present (and `kind !== "none"`), the platform emits a hidden
   * `__zs_raw__<col>` sibling at CREATE TABLE time carrying the
   * declared type and constraints, pre-computes the mask on every
   * write, and stores the mask in the field's OWN column. A read needs
   * no aliasing: the column with the declared name is the mask. The raw
   * sibling is NEVER part of the creator-visible SDK surface, and its
   * name is one `validate_field_name` refuses, so no filter, projection
   * or sort can name it either.
   *
   * `kind: "none"` is the explicit opt-out — encrypted columns where
   * the creator genuinely wants plaintext-on-read (e.g. background-
   * job-only read paths). It emits no second column and takes the
   * decrypt-on-read path, exactly as an unmasked encrypted field does.
   */
  mask?: {
    kind: MaskKind;
    classification: Classification;
  };
  /** Prefix used when this field declares a typedId assignment. */
  idPrefix?: string;

  /**
   * **Where this field physically lives** (runtime descriptor v2). See
   * [`FieldStorage`].
   *
   * Optional because `FieldDef` has two producers and only one of them knows the
   * answer: the migration fold emits it on every field of every collection, and the
   * `t.*()` authoring builders - which describe a field's type, not its storage - do
   * not. A consumer reading a v2 descriptor may rely on it being present; a consumer
   * reading a builder-authored `FieldDef` may not.
   */
  storage?: FieldStorage;
  /** Is this field readable through the creator-facing surface at all? */
  readable?: boolean;
  /** May a creator-facing filter name this field? */
  filterable?: boolean;
  /** May a creator-facing `orderBy` name this field? */
  sortable?: boolean;
  /** May a creator-facing projection return this field? */
  projectable?: boolean;
}

const TYPE_BUILDER_BRAND = Symbol.for("@zeroship/db/TypeBuilder");
const SCHEMA_BUILDER_BRAND = Symbol.for("@zeroship/db/SchemaBuilder");

/**
 * Fluent builder for a single field definition.
 *
 * Generic params:
 * - `T` — the inferred TS value type (bare primitive or branded id).
 * - `R` — `true` when the field was marked `.required()`; otherwise `false`.
 * - `M` — the mask kind declared via `.mask({...})`,
 *   or the default `"full"` for `t.encrypted()` columns, or
 *   `undefined` for unmasked columns. Surfaces through
 *   `InferFieldDef` so `Row<S>` wraps masked fields in
 *   `MaskedValue<T>` at the type level.
 * - `E` — `true` for encrypted fields, `undefined` for plain fields.
 *   `Filter<S>` excludes encrypted values.
 *
 * `t.string().required().min(3).max(50)` → `TypeBuilder<string, true>`
 * `t.encrypted()` → `TypeBuilder<string, false, "full", true, false>`
 * `t.string().mask({ kind: "email" })` → `TypeBuilder<string, false, "email", undefined, false>`
 * `t.string().required().default("x")` → `TypeBuilder<string, true, undefined, undefined, true>`
 */
type BuilderMetadata<T> = Pick<T, Extract<keyof T, "_assigned" | "_relation">>;
type RelationMetadata<N extends string> = [N] extends [never] ? {} : { readonly _relation: N };
type ReferenceValue<V, Target extends string> = V extends null ? null :
  Id<Target, V extends string ? string : V extends number ? number : V extends bigint ? bigint : never>;

export class TypeBuilder<
  T = unknown,
  R extends boolean = false,
  M extends MaskKind | undefined = undefined,
  E extends true | undefined = undefined,
  D extends boolean = false,
  F extends FilterKind = FilterKind,
> {
  /** @internal Type-level brand — do not access at runtime. */
  declare readonly _type: T;
  /** @internal Type-level brand for required/optional distinction. */
  declare readonly _required: R;
  /** @internal Type-level brand for the declared mask kind. */
  declare readonly _mask: M;
  /** @internal Type-level brand for encrypted-filter legality. */
  declare readonly _encryption: E;
  /** @internal Type-level brand for `.default()`-backed insert optionality. */
  declare readonly _hasDefault: D;
  /** @internal Type-level brand for portable filter operators. */
  declare readonly _filterKind: F;

  readonly [TYPE_BUILDER_BRAND] = true;

  private _def: FieldDef;

  static [Symbol.hasInstance](value: unknown): boolean {
    return Boolean(
      value &&
        typeof value === "object" &&
        (value as Record<PropertyKey, unknown>)[TYPE_BUILDER_BRAND] === true &&
        typeof (value as { toFieldDef?: unknown }).toFieldDef === "function",
    );
  }

  constructor(def: FieldDef) {
    this._def = { ...def };
  }

  /** Attach generated-column metadata after the value-type builder chain. */
  assigned(assign: ColumnAssignment): this & { readonly _assigned: true } {
    this._def.assign = assign;
    this._def.writable = false;
    return this as this & { readonly _assigned: true };
  }

  primaryKey(): this {
    this._def.primaryKey = true;
    return this;
  }

  /** Returns a frozen copy of the field definition. */
  toFieldDef(): Readonly<FieldDef> {
    return Object.freeze({ ...this._def });
  }

  /** Attach a reference without changing its scalar storage. */
  references<Target extends string, const N extends string = never>(
    this: [T] extends [IdValue | null] ? TypeBuilder<T, R, M, E, D, F> : never,
    table: Target,
    opts?: RefOptions & { relation?: N },
  ): TypeBuilder<ReferenceValue<T, Target>, R, M, E, D, F> & Pick<this, Extract<keyof this, "_assigned">> & RelationMetadata<N> {
    if (typeof table !== "string" || table.length === 0) {
      throw Object.assign(new Error("reference target must be a non-empty table name"), { code: "REF_EMPTY_TABLE" });
    }
    if (!["string", "text", "ref", "id", "int", "integer", "bigInt", "bigint"].includes(this._def.type)) {
      throw new TypeError("reference storage must be text or integer");
    }
    for (const key of ["refColumn", "relation", "onDelete", "onUpdate", "deferrable"] as const) delete this._def[key];
    this._def.refTarget = table;
    if (opts?.column !== undefined) this._def.refColumn = opts.column;
    if (opts?.relation !== undefined) this._def.relation = opts.relation;
    if (opts?.onDelete !== undefined) this._def.onDelete = opts.onDelete;
    if (opts?.onUpdate !== undefined) this._def.onUpdate = opts.onUpdate;
    if (opts?.deferrable !== undefined) this._def.deferrable = opts.deferrable;
    return this as unknown as TypeBuilder<ReferenceValue<T, Target>, R, M, E, D, F> & Pick<this, Extract<keyof this, "_assigned">> & RelationMetadata<N>;
  }

  /** Marks the field as required; validation will fail if the field is absent. */
  required(): TypeBuilder<T, true, M, E, D, F> & BuilderMetadata<this> {
    this._def.required = true;
    return this as unknown as TypeBuilder<T, true, M, E, D, F> & BuilderMetadata<this>;
  }

  /** Adds a unique index constraint to the field. */
  unique(): this {
    if (this._def.encrypted === true) {
      throw Object.assign(
        new Error(
          "t.encrypted().unique(): encrypted fields cannot enforce uniqueness.",
        ),
        { code: "UNIQUE_ENCRYPTED_UNSUPPORTED" as const },
      );
    }
    this._def.unique = true;
    return this;
  }

  /** Adds a non-unique index to the field for query performance. */
  index(): this {
    this._def.index = true;
    return this;
  }

  /** Sets the default value (or factory function) used when the field is absent on insert. */
  default(val: FieldDefaultValue | (() => FieldDefaultValue)): TypeBuilder<T, R, M, E, true, F> & BuilderMetadata<this> {
    this._def.default = val;
    return this as unknown as TypeBuilder<T, R, M, E, true, F> & BuilderMetadata<this>;
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
  enum<const Values extends readonly (T & (string | number))[]>(
    ...values: Values
  ): TypeBuilder<Values[number], R, M, E, D, F> & BuilderMetadata<this> {
    this._def.enum = [...values];
    return this as unknown as TypeBuilder<Values[number], R, M, E, D, F> & BuilderMetadata<this>;
  }

  /** For strings: a RegExp the value must match. */
  pattern(re: RegExp): this {
    this._def.pattern = re;
    return this;
  }

  /**
   * declare a column-level mask. The platform stores the
   * pre-computed mask in the field's OWN column and the real value in a
   * hidden `__zs_raw__<col>` sibling, so every query surface - filter,
   * projection, sort, `distinct`, `aggregate` - sees the mask, and the
   * SDK surfaces the field as `MaskedValue<T>`. The unmask round-trip
   * is the only path to plaintext, and the only reader of that sibling.
   *
   * A consequence worth knowing before you declare a mask:
   * `find({ ssn: "123-45-6789" })` matches nothing, and
   * `find({ ssn: { $gt: v } })` compares masks. Looking a row up by its
   * real value is `unmask`-shaped work, not filter-shaped work.
   *
   * Valid on `t.string()`, `t.number()`, `t.bytes()`, and
   * `t.encrypted()` (the encrypted column wraps one of those
   * primitive types). Refused on `t.ref()` with
   * `ENCRYPTED_ON_REF_UNSUPPORTED` — FK columns must remain
   * unencrypted/unmasked so the JOIN integrity check works (the
   * mask sibling would itself participate in the FK semantics,
   * which is incoherent).
   *
   * ```ts
   * const fields = {
   *   ssn:      t.encrypted(),           // → default mask = "full" + "pii"
   *   card_pan: t.encrypted().mask({ kind: "last4" }),         // → MaskedValue<string>
   *   email:    t.string().mask({ kind: "email" }),            // → MaskedValue<string>
   *   notes:    t.string().mask({ kind: "full", classification: "internal" }),
   *   opted:    t.encrypted().mask({ kind: "none" }),          // explicit no-mask
   * };
   * ```
   *
   * Defaults:
   * - `classification` defaults to `"pii"` when omitted.
   * - When `t.encrypted()` is declared without `.mask(...)`, the
   *   schema-normaliser auto-populates `{ kind: "full",
   *   classification: "pii" }` — fail-safe per §3 of the proposal.
   */
  mask<K extends MaskKind>(opts: { kind: K; classification?: Classification }): TypeBuilder<T, R, K, E, D, F extends "text" ? "text" : "equality"> & BuilderMetadata<this> {
    if (opts === null || typeof opts !== "object") {
      throw Object.assign(
        new Error(".mask(opts): opts must be an object with at least `{ kind }`"),
        { code: "MASK_INVALID_OPTS" as const },
      );
    }
    const kind = opts.kind;
    const VALID_KINDS: ReadonlySet<MaskKind> = new Set<MaskKind>([
      "full",
      "last4",
      "first4",
      "email",
      "name",
      "date-year",
      "date-decade",
      "none",
    ]);
    if (!VALID_KINDS.has(kind)) {
      throw Object.assign(
        new Error(
          `.mask({ kind }): kind must be one of ${[...VALID_KINDS].join(" | ")}, got "${String(kind)}"`,
        ),
        { code: "MASK_INVALID_KIND" as const },
      );
    }
    const classification = opts.classification ?? "pii";
    const VALID_CLASSIFICATIONS: ReadonlySet<Classification> = new Set<Classification>([
      "public",
      "pii",
      "spi",
      "phi",
      "pci",
      "internal",
    ]);
    if (!VALID_CLASSIFICATIONS.has(classification)) {
      throw Object.assign(
        new Error(
          `.mask({ classification }): classification must be one of ${[...VALID_CLASSIFICATIONS].join(" | ")}, got "${String(classification)}"`,
        ),
        { code: "MASK_INVALID_CLASSIFICATION" as const },
      );
    }
    // Reference columns must remain visible to foreign-key joins.
    if (this._def.type === "ref") {
      throw Object.assign(
        new Error(
          ".mask(): not supported on t.ref() fields (FK columns must stay unmasked for JOIN integrity)",
        ),
        { code: "ENCRYPTED_ON_REF_UNSUPPORTED" as const },
      );
    }
    // Mask only makes sense on column types whose stored value is
    // a single primitive (string / number / bytes). Refuse on
    // array / object / json / vector / geoPoint / union / literal /
    // boolean / date / calendarDate — these either have no stable
    // textual masked form or aren't yet supported.
    const VALID_TYPES: ReadonlySet<TypeName> = new Set<TypeName>(["string", "number", "bytes"]);
    if (!VALID_TYPES.has(this._def.type)) {
      throw Object.assign(
        new Error(
          `.mask(): only valid on string / number / bytes wrapped types, got "${this._def.type}"`,
        ),
        { code: "MASK_ON_UNSUPPORTED_TYPE" as const },
      );
    }
    this._def.mask = { kind, classification };
    return this as unknown as TypeBuilder<T, R, K, E, D, F extends "text" ? "text" : "equality"> & BuilderMetadata<this>;
  }

  /** Allow null in the field's value type. */
  nullable(): TypeBuilder<T | null, R, M, E, D, F> & BuilderMetadata<this> {
    return this as TypeBuilder<T | null, R, M, E, D, F> & BuilderMetadata<this>;
  }

  /** Assign the database timestamp on insert. */
  auto_now(): this & { readonly _assigned: true } {
    if (this._def.type !== "date") {
      throw Object.assign(
        new Error(
          `.auto_now(): only valid on t.timestamp() fields, got "${this._def.type}"`,
        ),
        { code: "AUTO_NOW_ON_NON_TIMESTAMP" as const },
      );
    }
    return this.assigned({ by: "now", on: "insert" });
  }

  /** Assign the database timestamp on insert and subsequent writes. */
  auto_now_on_update(): this & { readonly _assigned: true } {
    if (this._def.type !== "date") {
      throw Object.assign(
        new Error(
          `.auto_now_on_update(): only valid on t.timestamp() fields, got "${this._def.type}"`,
        ),
        { code: "AUTO_NOW_ON_NON_TIMESTAMP" as const },
      );
    }
    return this.assigned({ by: "now", on: "write" });
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
  string(): TypeBuilder<string, false, undefined, undefined, false, "text"> {
    return new TypeBuilder<string, false, undefined, undefined, false, "text">({ type: "string" });
  },
  /** Creates an integer field definition. */
  integer(): TypeBuilder<number, false, undefined, undefined, false, "ordered"> {
    return new TypeBuilder<number, false, undefined, undefined, false, "ordered">({ type: "integer" });
  },
  /** Creates a number field definition. */
  number(): TypeBuilder<number, false, undefined, undefined, false, "ordered"> {
    return new TypeBuilder<number, false, undefined, undefined, false, "ordered">({ type: "number" });
  },
  /** Creates a fixed precision decimal represented as exact text. */
  numeric(opts: { precision?: number; scale?: number } = {}): TypeBuilder<Decimal, false, undefined, undefined, false, "exact"> {
    const precision = opts.precision ?? 38;
    const scale = opts.scale ?? 9;
    if (!Number.isSafeInteger(precision) || precision < 1 || precision > 1000) {
      throw new TypeError("t.numeric precision is outside the portable range");
    }
    if (!Number.isSafeInteger(scale) || scale < 0 || scale > precision) {
      throw new TypeError("t.numeric scale must be between zero and precision");
    }
    return new TypeBuilder<Decimal, false, undefined, undefined, false, "exact">({
      type: "number",
      precision,
      scale,
    });
  },
  /** Creates an integer field with exact bigint input and output beyond the safe number range. */
  bigInt(): TypeBuilder<number | bigint, false, undefined, undefined, false, "ordered"> {
    return new TypeBuilder<number | bigint, false, undefined, undefined, false, "ordered">({ type: "bigInt" });
  },
  /** Creates a boolean field definition. */
  boolean(): TypeBuilder<boolean, false, undefined, undefined, false, "equality"> {
    return new TypeBuilder<boolean, false, undefined, undefined, false, "equality">({ type: "boolean" });
  },
  /**
   * Creates a timestamp field — `TIMESTAMPTZ` in Postgres, Unix-ms
   * `number` at the JS layer. Accepts `Date`, ISO string, or `number`
   * on input (the shared ORM normalizes using the descriptor). Reads come back as
   * `number` (millisecond epoch). For wall-clock dates without a
   * time-of-day component, use {@link calendarDate} instead.
   */
  timestamp(): TypeBuilder<number, false, undefined, undefined, false, "ordered"> {
    return new TypeBuilder<number, false, undefined, undefined, false, "ordered">({ type: "date" });
  },
  /** Creates a JSON field definition for objects, arrays, and scalars. */
  json(): TypeBuilder<JsonValue, false, undefined, undefined, false, "json"> {
    return new TypeBuilder<JsonValue, false, undefined, undefined, false, "json">({ type: "json" });
  },
  /**
   * Creates an array field definition. Pass the item type builder as the argument:
   * `t.array(t.string())` produces `{ type: "array", items: "string" }`.
   *
   * Only primitive item types are supported today (`string`, `number`,
   * `boolean`, `date`, `json`, `calendarDate`). Passing `t.ref(...)`,
   * `t.object({...})`, `t.union(...)`, `t.literal(...)` or a nested
   * `t.array(...)` throws synchronously with code
   * `INVALID_ARRAY_ITEM` — the previous unchecked cast silently produced
   * malformed `FieldDef`s (e.g. dropping `refTarget` so `validateRefTargets`
   * could not visit array items).
   */
  array<U>(items: TypeBuilder<U, any, any, any, any>): TypeBuilder<U[], false, undefined, undefined, false, "json"> {
    if (!(items instanceof TypeBuilder)) {
      throw Object.assign(
        new Error("t.array(items) requires a TypeBuilder (use t.string(), t.number(), ...)"),
        { code: "INVALID_ARRAY_ITEM" as const },
      );
    }
    const itemDef = items.toFieldDef();
    const PRIMITIVE_ITEM_TYPES: ReadonlySet<string> = new Set([
      "string", "number", "boolean", "date", "json", "calendarDate",
    ]);
    if (!PRIMITIVE_ITEM_TYPES.has(itemDef.type)) {
      throw Object.assign(
        new Error(
          `t.array(items): item type "${itemDef.type}" is not supported — ` +
            `only primitive item types are allowed (string, number, boolean, ` +
            `date, json, calendarDate). Storing arrays of refs/objects/unions ` +
            `is not implemented; model it as a separate collection with a ref.`,
        ),
        { code: "INVALID_ARRAY_ITEM" as const },
      );
    }
    const itemType = itemDef.type as PrimitiveTypeName;
    return new TypeBuilder<U[], false, undefined, undefined, false, "json">({ type: "array", items: itemType });
  },
  /** Declare a branded reference, optionally naming its target column and FK actions. */
  ref<T extends string, const N extends string = never>(table: T, opts?: RefOptions & { relation?: N }): TypeBuilder<Id<T>, false, undefined, undefined, false, "text"> & RelationMetadata<N> {
    return new TypeBuilder<string, false, undefined, undefined, false, "text">({ type: "ref" }).references(table, opts);
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
  object<S extends Record<string, TypeBuilder<any, any, any, any, any>>>(shape: S): TypeBuilder<InferSchema<S>, false, undefined, undefined, false, "json"> {
    if (shape === null || typeof shape !== "object" || Array.isArray(shape)) {
      throw Object.assign(
        new Error("t.object(shape) requires a record of nested type builders"),
        { code: "OBJECT_INVALID_SHAPE" as const },
      );
    }
    const nested: Record<string, FieldDef> = {};
    for (const [key, val] of Object.entries(shape)) {
      if (!(val instanceof TypeBuilder)) {
        throw Object.assign(
          new Error(`t.object: nested field "${key}" must be a TypeBuilder (use t.string(), t.number(), ...)`),
          { code: "OBJECT_FIELD_NOT_TYPEBUILDER" as const },
        );
      }
      nested[key] = { ...val.toFieldDef() };
    }
    return new TypeBuilder<InferSchema<S>, false, undefined, undefined, false, "json">({ type: "object", shape: nested });
  },
  /**
   * vector embedding field. Stored as pgvector's
   * `vector(N)` column type on PG; the runtime materialises a matching
   * ivfflat index per the chosen metric.
   *
   * ```ts
   * embedding: t.vector(1536), // OpenAI text-embedding-3-small
   * embedding: t.vector(768, { metric: "l2" }),
   * ```
   *
   * **Dim range**: 1..=16000 (pgvector hard ceiling). Out-of-range
   * values throw synchronously at schema-definition time with code
   * `VECTOR_INVALID_DIMS`.
   *
   * **Default metric**: `"cosine"` — the convention for normalised
   * embedding models (OpenAI, Cohere, …). Override via
   * `{ metric: "l2" | "innerProduct" }`.
   *
   * At runtime the column is stored as a typed `number[]`; reads come
   * back as the same shape. Use `collection.search({ vector, k })` for
   * nearest-neighbour queries.
   */
  vector(dims: number, opts?: { metric?: VectorMetric }): TypeBuilder<number[], false, undefined, undefined, false, "search"> {
    if (typeof dims !== "number" || !Number.isInteger(dims) || dims < 1 || dims > 16000) {
      throw Object.assign(
        new Error(
          `t.vector(dims): dims must be an integer in 1..=16000 (pgvector hard ceiling), got ${dims}`,
        ),
        { code: "VECTOR_INVALID_DIMS" as const },
      );
    }
    const metric: VectorMetric = opts?.metric ?? "cosine";
    if (metric !== "cosine" && metric !== "l2" && metric !== "innerProduct") {
      throw Object.assign(
        new Error(
          `t.vector(dims, { metric }): metric must be "cosine" | "l2" | "innerProduct", got "${String(metric)}"`,
        ),
        { code: "VECTOR_INVALID_METRIC" as const },
      );
    }
    return new TypeBuilder<number[], false, undefined, undefined, false, "search">({
      type: "vector",
      vectorDims: dims,
      vectorMetric: metric,
    });
  },
  /**
   * geographic point field (WGS84, EPSG:4326). Stored as
   * PostGIS's `geography(POINT, 4326)` column on PostgreSQL and as a packed
   * coordinate pair on SQLite.
   *
   * ```ts
   * const fields = {
   *   location: t.geoPoint().required(),
   * };
   * // Insert / read shape: { lat: number, lng: number }
   * await db.places.insert({ location: { lat: 51.5074, lng: -0.1278 } });
   * ```
   *
   * Query via `collection.near({ field, point, radius })` for spatial
   * within-radius search. `radius` is in metres on both backends.
   *
   * **Note on PG**: the column type requires PostGIS to be installed on
   * the database; the runtime probes `pg_extension WHERE extname='postgis'`
   * and surfaces a typed `POSTGIS_EXTENSION_MISSING` error when absent.
   */
  geoPoint(): TypeBuilder<{ lat: number; lng: number }, false, undefined, undefined, false, "search"> {
    return new TypeBuilder<{ lat: number; lng: number }, false, undefined, undefined, false, "search">({ type: "geoPoint" });
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
  calendarDate(): TypeBuilder<string, false, undefined, undefined, false, "ordered"> {
    return new TypeBuilder<string, false, undefined, undefined, false, "ordered">({ type: "calendarDate" });
  },
  /** Native binary column, also usable as an encrypted field's wrap. */
  bytes(): TypeBuilder<Uint8Array, false, undefined, undefined, false, "equality"> {
    return new TypeBuilder<Uint8Array, false, undefined, undefined, false, "equality">({ type: "bytes" });
  },
  /**
   * Encrypt string, number or byte values with a fresh random nonce per write.
   * The ciphertext is bound to its collection, column and row identity.
   * Encrypted fields cannot be filtered, sorted, or unique.
   * A full mask is applied by default; `.mask({ kind: "none" })` opts out.
   *
   * ```ts
   * const fields = {
   *   ssn: t.encrypted().required(),
   *   payload: t.encrypted({ of: t.bytes() }),
   *   amount: t.encrypted({ of: t.number() }),
   * };
   * ```
   */
  encrypted<
    // Infer the plaintext type from the wrapped builder.
    W extends TypeBuilder<string | number | bigint | Uint8Array, any, any, any, any> | undefined = undefined,
    T extends string | number | bigint | Uint8Array =
      W extends TypeBuilder<infer WT extends string | number | bigint | Uint8Array, any, any, any, any>
        ? WT
        : string,
  >(
    opts?: Omit<EncryptedFieldOpts, "of"> & { of?: W },
  ): TypeBuilder<T, false, "full", true, false> {
    const innerBuilder = opts?.of;
    let plaintextType: "string" | "number" | "bytes" = "string";
    let precision: number | undefined;
    let scale: number | undefined;
    if (innerBuilder !== undefined) {
      if (!(innerBuilder instanceof TypeBuilder)) {
        throw Object.assign(
          new Error("t.encrypted({ of }): of must be a TypeBuilder (t.string() / t.number() / t.bytes())"),
          { code: "ENCRYPTED_TYPE_UNSUPPORTED" as const },
        );
      }
      const def = innerBuilder.toFieldDef();
      if (def.type === "string") plaintextType = "string";
      else if (def.type === "number") {
        plaintextType = "number";
        precision = def.precision;
        scale = def.scale;
      } else if (def.type === "bytes") plaintextType = "bytes";
      else {
        throw Object.assign(
          new Error(
            `t.encrypted({ of }): only string / number / bytes are supported, got "${def.type}"`,
          ),
          { code: "ENCRYPTED_TYPE_UNSUPPORTED" as const },
        );
      }
    }
    // `type` describes the plaintext; `encrypted` selects binary storage.
    // fail-safe default-mask rule (§3 of the
    // sensitive-field-masking proposal): every `t.encrypted()` column
    // gets `mask: { kind: "full", classification: "pii" }` at
    // builder time when no `.mask(...)` is chained. The
    // intentional path to plaintext-on-read is the explicit
    // `.mask({ kind: "none" })` opt-out. Chaining `.mask({...})`
    // after `t.encrypted()` overwrites this default via the
    // builder's `.mask` method (assigns `_def.mask` unconditionally).
    return new TypeBuilder<T, false, "full", true>({
      type: plaintextType,
      ...(precision === undefined ? {} : { precision, scale }),
      encrypted: true,
      mask: { kind: "full", classification: "pii" },
    });
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
        { code: "LITERAL_NULL_VALUE" as const },
      );
    }
    const ty = typeof value;
    if (ty !== "string" && ty !== "number" && ty !== "boolean") {
      throw Object.assign(
        new Error(
          `t.literal(value): value must be string | number | boolean, got ${ty}`,
        ),
        { code: "LITERAL_INVALID_TYPE" as const },
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
  /** A textual identifier type; generation requires a typedId assignment. */
  id(prefix?: string): TypeBuilder<string> {
    if (prefix !== undefined) {
      if (typeof prefix !== "string" || prefix.length === 0) {
        throw Object.assign(
          new Error("t.id(prefix): prefix must be a non-empty string"),
          { code: "ID_INVALID_PREFIX" as const },
        );
      }
      if (!/^[a-z][a-z0-9_]*$/.test(prefix)) {
        throw Object.assign(
          new Error(
            `t.id(prefix): prefix must match /^[a-z][a-z0-9_]*$/ (got "${prefix}")`,
          ),
          { code: "ID_INVALID_PREFIX" as const },
        );
      }
      // `usr` is the platform user-id prefix (`crates/zeroship-core/src/typed_id.rs`);
      // reserve it so a creator id can never collide with a platform user id.
      // The generated migration surface mirrors this fence.
      if (prefix === "usr") {
        throw Object.assign(
          new Error(
            `t.id(prefix): "usr" is reserved for platform user ids; choose a different prefix`,
          ),
          { code: "ID_RESERVED_PREFIX" as const },
        );
      }
    }
    const def: FieldDef = { type: "id" };
    if (prefix !== undefined) def.idPrefix = prefix;
    return new TypeBuilder<string>(def);
  },
  /** Assign the request actor on insert, or null for anonymous writes. */
  actor(): TypeBuilder<string | null> & { readonly _assigned: true } {
    return new TypeBuilder<string | null>({ type: "string" }).assigned({ by: "actor", on: "insert" });
  },
  union<V extends readonly TypeBuilder<any, any, any, any, any>[]>(...variants: V): TypeBuilder<InferUnion<V>, false, undefined, undefined, false, "json"> {
    if (variants.length < 2) {
      throw Object.assign(
        new Error(
          `t.union(...) requires at least 2 variants, got ${variants.length}`,
        ),
        { code: "UNION_TOO_FEW_VARIANTS" as const },
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
          { code: "UNION_VARIANT_NOT_TYPEBUILDER" as const },
        );
      }
      const def = v.toFieldDef();
      if (def.type !== "object" || def.shape === undefined) {
        throw Object.assign(
          new Error(
            `t.union: variant #${i} must be a t.object(...) (got type "${def.type}")`,
          ),
          { code: "UNION_VARIANT_NOT_OBJECT" as const },
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
    return new TypeBuilder<InferUnion<V>, false, undefined, undefined, false, "json">({
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
      { code: "UNION_NO_VARIANTS" as const },
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
      { code: "UNION_NO_DISCRIMINATOR" as const },
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
      { code: "UNION_DISCRIMINATOR_OVERLAP" as const },
    );
  }
  if (distinctCandidates.length > 1) {
    throw Object.assign(
      new Error(
        `t.union: ambiguous discriminator — multiple candidate keys with distinct literals: ${distinctCandidates.join(", ")}. Use only one literal field per variant or rename one of them.`,
      ),
      { code: "UNION_DISCRIMINATOR_AMBIGUOUS" as const },
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
export type InferUnion<V extends readonly TypeBuilder<any, any, any, any, any>[]> =
  V[number] extends TypeBuilder<infer U, any, any, any, any> ? U : never;

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
