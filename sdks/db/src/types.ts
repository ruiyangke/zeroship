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

/**
 * Infers the value type from a field definition built via `t.*`.
 *
 * **P5.5 PR 1** — when the field is masked (third `TypeBuilder` brand
 * is a non-`"none"` mask kind), the inferred type wraps the bare
 * primitive in `MaskedValue<T>`. The `<col>_masked` sibling column
 * is NEVER part of `Row<S>` — only the parent column appears, with
 * the masked-value wrapper around it.
 */
export type InferFieldDef<T> =
  T extends TypeBuilder<infer U, any, infer M, any, any>
    ? M extends MaskKind
      ? M extends "none"
        ? U
        : U extends string | number | Uint8Array
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

/** Keys that are not explicitly required. */
export type OptionalKeys<S> = Exclude<keyof S, RequiredKeys<S>>;

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
        [K in keyof S]-?: NonNullable<S[K]> extends TypeBuilder<any, any, any, any, any> ? true : false;
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

/** Insert-time shape inference. Required + defaulted fields become optional. */
export type InferInsertSchema<S> = S extends infer T
  ? IsSchemaDict<T> extends true
    ? {
        [K in InsertRequiredKeys<T>]: InferFieldDef<T[K]>;
      } & {
        [K in InsertOptionalKeys<T>]?: InferFieldDef<T[K]>;
      }
    : T
  : never;

/**
 * **P7 PR 1** — platform-managed system fields injected into every
 * `Row<S>`. Mirrors `SYSTEM_FIELD_NAMES` on the Rust side
 * (`crates/plugin-db/src/query.rs`). Creator schemas cannot declare
 * fields with these names — the SDK-side reservation in
 * `@zeroship/bootstrap/install-schema` and the Rust-side validator
 * in `validate_field_name_for_declaration` enforce the fence at
 * register-model time.
 *
 * Wire types (PR 3 — canonical typed_id + ISO-8601-friendly shape;
 * PR 1 stub of `number` for `id` is widened here in lockstep with
 * the Rust `dispatch_insert` auto-mint pass):
 * - `id` — `string` typed_id (`<prefix>_<22 base62 chars>`); the
 *   `dispatch_insert` path on the Rust side mints fresh ids via
 *   `zeroship_core::typed_id::generate(prefix)` when the inbound
 *   row omits one. Pre-P7 collections that still store integer ids
 *   migrate via PR 6's one-time ALTER pass; until then the SDK's
 *   `Collection._loadById` accepts either shape on the wire but
 *   exposes `string` on `Row<S>`.
 * - `created_at` / `updated_at` — Unix-ms `number`. The wire widens
 *   to ISO 8601 strings in a later PR (P7.5 / P8); the current shape
 *   stays a number to avoid a Date-parse cost on every read.
 * - `created_by` / `updated_by` — nullable actor typed_id string,
 *   `null` for system-initiated writes (migrations, background
 *   jobs). PR 3 wires the auto-populate from the per-request
 *   user context (`RuntimeState::per_request_user`).
 * - `version` — monotonic integer; starts at 1, bumped by 1 on every
 *   UPDATE. PR 4 wires the bump + optimistic-concurrency CAS.
 * - `deleted_at` — nullable timestamp `number`; `null` for live rows.
 *   PR 5 wires `delete()` to set this field and `find()` to
 *   auto-filter `deleted_at IS NULL`.
 *
 * The fields are appended after the user's schema so chain methods
 * on the inferred row see the user shape first (matches the source
 * order: every creator table gets the system fields as
 * platform-injected, not creator-declared).
 */
export type SystemFields = {
  id: string;
  created_at: number;
  updated_at: number;
  created_by: string | null;
  updated_by: string | null;
  version: number;
  deleted_at: number | null;
};

/**
 * The persisted row type: user fields + 7 platform-managed system
 * fields (`id`, `created_at`, `updated_at`, `created_by`, `updated_by`,
 * `version`, `deleted_at`). Extends `InferSchema` so required user
 * fields remain required; system fields are always present at read
 * time (auto-populated by the platform).
 *
 * **P7 PR 3** — `id` is a typed_id string
 * (`<prefix>_<base62(uuidv7)>`) and the system fields are exposed in
 * snake_case only.
 */
export type Row<S> = InferSchema<S> & SystemFields;

/** Input type accepted by `insert()` / `upsert()` — required fields
 * stay required, auto-populated system fields are excluded so the
 * platform mints them (PR 3). */
export type RowInput<S> = InferInsertSchema<S> & {
  id?: never;
  created_at?: never;
  updated_at?: never;
  created_by?: never;
  updated_by?: never;
  version?: never;
  deleted_at?: never;
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

/** Equality operators allowed on deterministic-encrypted fields. */
type EqualityOnlyOps<T> = {
  $eq?: T;
  $in?: T[];
};

/** String-specific operators. */
type StringOps = {
  $like?: string;
  $ilike?: string;
  $search?: string;
};

/**
 * Filter value for an ordinary field — either a direct value, null, or
 * operator object.
 *
 * `$like` / `$ilike` / `$search` are real backend operators: plugin-db's
 * query builder validates them and lowers them to SQL / FTS predicates.
 */
type PlainFilterValue<T> =
  T | null |
  (NonNullable<T> extends string ? ComparisonOps<NonNullable<T>> & StringOps :
   NonNullable<T> extends number ? ComparisonOps<NonNullable<T>> :
   NonNullable<T> extends boolean ? ComparisonOps<NonNullable<T>> :
   ComparisonOps<NonNullable<T>>);

/** Filter value for a deterministic-encrypted field — equality only. */
type DeterministicEncryptedFilterValue<T> =
  T | null | EqualityOnlyOps<NonNullable<T>>;

/**
 * Filter legality derived from the schema field builder itself.
 *
 * This keeps the type layer aligned with the runtime fence in
 * `validateEncryptedFieldsInFilter`:
 * - randomised-encrypted fields are un-filterable
 * - deterministic-encrypted fields accept only bare equality / `$eq` / `$in`
 * - plain fields keep the normal operator surface
 */
type FilterValueForFieldBuilder<F> =
  F extends TypeBuilder<infer U, any, any, infer E, any>
    ? E extends "randomised"
      ? never
      : E extends "deterministic"
        ? DeterministicEncryptedFilterValue<NonNullable<U>>
        : PlainFilterValue<NonNullable<U>>
    : never;

type FilterValueForKey<S, K extends keyof Row<S>> =
  IsSchemaDict<S> extends true
    ? K extends keyof S
      ? FilterValueForFieldBuilder<NonNullable<S[K]>>
      : PlainFilterValue<NonNullable<Row<S>[K]>>
    : PlainFilterValue<NonNullable<Row<S>[K]>>;

/** Typed filter for a document — each field accepts its value type or operators.
 *  Field values use `NonNullable<Row<S>[K]>` so `undefined` is rejected at the
 *  type layer; pass `null` to match SQL NULL explicitly. */
export type Filter<S> = {
  [K in keyof Row<S>]?: FilterValueForKey<S, K>
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
// InferRow / InferRowInput / InferId — type-only helpers that pull `Row` /
// `RowInput` / `Id` out of a
// Collection without `Parameters<typeof col.insertMany>[0][number]` plumbing.
// ---------------------------------------------------------------------------

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
  X extends TypeBuilder<infer U, any, any, any, any>
    ? U extends Id<infer T>
      ? T
      : never
    : X extends { type: "ref"; refTarget: infer T extends string }
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
 * and the parent db's schema map. Falls back to `PlainObject` when the
 * target name can't be matched against any declared collection — that
 * preserves the v1 behaviour for unknown targets without breaking
 * compilation. Tightens to the real `Row<TargetSchema>` whenever
 * `installSchema`'s schema map carries the target name (the common case).
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
 * `AllSchemas` is the schema map that `installSchema` was given —
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

/**
 * **P5 PR 2** — column-encryption mode. Picks both the nonce-derivation
 * strategy and the AAD shape (Camp A, resolved 2026-05-24):
 *
 * - `randomised` — per-write fresh nonce; AAD binds `(collection,
 *   column, row_pk)`. Two encrypts of the same plaintext produce
 *   different ciphertext (fail-safe default). The SDK refuses ANY
 *   filter on a randomised column at the boundary because no
 *   equality-on-ciphertext lookup can match.
 * - `deterministic` — synthetic nonce HMAC-derived from the plaintext;
 *   AAD binds `(collection, column)` only. Same plaintext under the
 *   same column produces identical ciphertext, enabling B-tree
 *   equality lookups. The SDK refuses range / regex / LIKE on
 *   deterministic columns (only `$eq`/`$in`). Inherits the standard
 *   deterministic-mode leak — equality across rows is observable to
 *   anyone with column read access.
 */
export type EncryptionMode = "randomised" | "deterministic";

/**
 * **P5.5 PR 1** — built-in mask transform applied at write time to
 * compute the sibling `<col>_masked` column's value from the
 * plaintext. Mirrors `crate::diff::MaskKind` on the Rust side.
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
 * mask kind is a platform PR, not creator config.
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
 * **P5.5 PR 1** — sensitivity classification used by the unmask
 * authorization (PR 4) and audit (PR 4) machinery. Mirrors
 * `crate::diff::Classification` on the Rust side.
 *
 * - `public`   — usernames, display names, public profile data.
 * - `pii`      — full name, email, address, phone, IP, date of birth.
 *                Default classification for encrypted columns without
 *                explicit `.mask(...)`.
 * - `spi`      — SSN, driver's license, biometric data (CPRA
 *                "sensitive PI").
 * - `phi`      — health records, medical IDs, diagnosis (HIPAA scope).
 * - `pci`      — card numbers, CVV, magnetic stripe (PCI-DSS scope).
 * - `internal` — platform-internal metadata, system-field overrides.
 *
 * The six names are also reserved as column names by
 * `crates/plugin-db/src/query.rs::validate_field_name` so creator
 * schemas cannot accidentally collide with the taxonomy.
 */
export type Classification =
  | "public"
  | "pii"
  | "spi"
  | "phi"
  | "pci"
  | "internal";

/**
 * **P5.5 PR 1** — options accepted by `.mask(opts)` on a `TypeBuilder`.
 *
 * - `kind`            — required. The mask transform; see {@link MaskKind}.
 * - `classification`  — optional. Defaults to `"pii"` when omitted.
 *                       Drives unmask authorization (PR 4).
 */
export interface MaskOpts {
  kind: MaskKind;
  classification?: Classification;
}

/**
 * **P5.5 PR 1** — wire shape the Rust read path emits for masked
 * columns (the `__zsmask__` sentinel object).
 *
 * **P9 PR 2** — this wire shape is now consumed entirely Rust-side:
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

/**
 * **P5.5 PR 1** — opaque actor descriptor passed to
 * `MaskedValue.unmask({ actor? })`. PR 4 will wire the round-trip
 * through the unmask RPC; today the type stays minimal (any plain
 * object) so PR 5 (`defineMaskPolicy`) can land the concrete shape.
 */
export type Actor = Record<string, unknown>;

/**
 * **P9 PR 2** — masked-value wrapper, now a NATIVE v8_class.
 *
 * `MaskedValue` instances are minted Rust-side by the row serializer's
 * rehydration pass (`crates/plugin-db/src/v8_classes/masked_value.rs`)
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
export declare class MaskedValue<T extends string | number | Uint8Array = string> {
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
   * For `wraps = bytes` the plaintext arrives base64-encoded (caller
   * decodes with `Uint8Array.from(atob(pt), c => c.charCodeAt(0))`); for
   * `wraps = number` it arrives as a stringified f64.
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
 * **P5 PR 2** — options accepted by `t.encrypted(opts?)`.
 *
 * - `mode` — defaults to `"randomised"` (fail-safe).
 * - `keyId` — selects the per-platform root key (env var
 *   `ZEROSHIP_COLUMN_KEY_<KEYID>` or `__zeroship_admin.column_keys`).
 *   Defaults to `"default"`.
 * - `wraps` — the inner primitive type, ONE OF `t.string()` /
 *   `t.number()` / `t.bytes()` (the `bytes` wrap accepts base64-encoded
 *   string at the JS layer). Defaults to `t.string()`. Other types
 *   throw synchronously with `ENCRYPTED_WRAPS_UNSUPPORTED`.
 */
export interface EncryptedFieldOpts<Mode extends EncryptionMode = EncryptionMode> {
  /** Encryption mode. Defaults to `"randomised"`. */
  mode?: Mode;
  /** Key id selecting the per-platform root. Defaults to `"default"`. */
  keyId?: string;
  /**
   * Inner type the encrypted value wraps. Only string / number / bytes
   * are supported. Passing any other `TypeBuilder` throws with code
   * `ENCRYPTED_WRAPS_UNSUPPORTED` at schema-definition time.
   */
  wraps?: TypeBuilder<any, any, any, any, any>;
}
/** Definition for an array field with a declared item type. */
export type ArrayTypeDef = { type: "array"; items: PrimitiveTypeName };
/**
 * All supported type names. Includes "array", "ref" (B2 typed FK),
 * "object" (D2 nested validators), "calendarDate" (D3 — `YYYY-MM-DD`),
 * "literal" (C2 discriminator constant), and "union" (C2 discriminated
 * union document shape — proposal §C2).
 *
 * **P7 PR 1** — adds `"id"` (typed_id PK candidate) and `"actor"`
 * (session.actor_id source for `created_by` / `updated_by` style
 * columns). These shapes feed the PR 2 CREATE TABLE rewrite that
 * injects the seven platform system fields; PR 1 ships the builders
 * + wire discriminators only.
 */
export type TypeName = PrimitiveTypeName | "array" | "ref" | "object" | "literal" | "union" | "vector" | "geoPoint" | "bytes" | "id" | "actor";

/**
 * **P4 PR 2** — distance metric for `t.vector(...)` fields. The three
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
 * Cross-table typed ID (B2). Stored as a TEXT typed_id (`<prefix>_<22
 * base62 chars>`) at the DB layer but brand-tagged at the type layer
 * so `Id<"users">` and `Id<"posts">` are mutually incompatible — typos
 * like `db.posts.get({ authorId: postId })` (where `postId` is
 * `Id<"posts">`) become compile errors.
 *
 * Modelled after Convex's `Id<TableName>` brand
 * ([docs.convex.dev/database/document-ids]). The brand is a phantom
 * property typed but never assigned at runtime; the runtime value is
 * just a string, so JSON serialisation is unchanged.
 *
 * **P7 PR 3** — widened from `number & { __zeroshipTable }` to
 * `string & { __zeroshipTable }` in lockstep with the Rust-side
 * `id TEXT PRIMARY KEY` DDL and the `dispatch_insert` auto-mint pass
 * (which calls `zeroship_core::typed_id::generate(prefix)`). FK
 * columns also cascade to TEXT (`def_to_pg_type` for `Some("ref")`),
 * so a brand-typed `authorId: Id<"users">` round-trips faithfully.
 */
export type Id<T extends string> = string & {
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
  /**
   * **P4 PR 2** — declared dimensionality of a `t.vector(...)` field.
   * Present iff `type === "vector"`. The DDL emitter (PG arm) renders
   * `vector(N)` with this value; the index emitter routes through
   * `VectorIndex::ensure_vector_index`. Range: `1..=16000` (pgvector
   * hard ceiling).
   */
  vectorDims?: number;
  /**
   * **P4 PR 2** — distance metric for a `t.vector(...)` field. Present
   * iff `type === "vector"`. Selects the pgvector opclass for the
   * ivfflat index and the operator for ORDER BY at search time.
   */
  vectorMetric?: VectorMetric;
  /**
   * **P4 PR 3** — full-text-search marker. Set to `true` by the
   * `.fts(language?)` modifier on a `t.string()` field. Every field
   * carrying this flag is folded into a single composite FTS index per
   * collection (Q-P4-B); the index emitter (`build_create_indexes` on
   * the Rust side) walks all `fts === true` fields and emits one
   * `IndexSpec { kind: Fts { language } }` covering them in declared
   * order. Reject on non-string fields with code `FTS_ON_NON_STRING`.
   */
  fts?: boolean;
  /**
   * **P4 PR 3** — tsvector configuration language for an FTS-marked
   * column. Honoured on PG (`to_tsvector('pg_catalog.<lang>', ...)`);
   * SQLite FTS5 default tokenizer is language-agnostic Unicode and
   * ignores it. Defaults to `"english"` when `.fts()` is called without
   * an explicit argument.
   */
  ftsLanguage?: string;
  /**
   * **P5 PR 2** — column-encryption metadata. Present iff the SDK
   * declared the column with `t.encrypted({ mode, keyId, wraps })`.
   * The DDL emitter renders BYTEA / BLOB regardless of `wraps`; the
   * `wraps` field survives so the validator walks the right
   * type-checker before the encrypt pass swaps bytes in.
   *
   * - `mode` — `"randomised"` (default, fail-safe) or `"deterministic"`
   *   (enables B-tree equality lookups; carries the standard
   *   deterministic-mode leak).
   * - `keyId` — selects the per-platform root key. Defaults to
   *   `"default"`.
   * - `wraps` — the inner primitive (`"string"` | `"number"` | `"bytes"`).
   *   Other types are refused at schema-definition time with code
   *   `ENCRYPTED_WRAPS_UNSUPPORTED`.
   */
  encrypted?: {
    mode: EncryptionMode;
    keyId: string;
    wraps: "string" | "number" | "bytes";
  };
  /**
   * **P5.5 PR 1** — column-mask metadata. Present iff the SDK declared
   * the column with `.mask({ kind, classification? })`, OR the column
   * is `t.encrypted(...)` without explicit `.mask(...)` and the
   * schema-normaliser auto-populates the default mask
   * (`{ kind: "full", classification: "pii" }`).
   *
   * When present (and `kind !== "none"`), the platform emits a hidden
   * `<col>_masked` sibling column at CREATE TABLE time (PR 2),
   * pre-computes the masked representation on every write (PR 2), and
   * aliases the sibling back to the schema-declared name on read
   * (PR 3). The sibling column is NEVER part of the creator-visible
   * SDK surface — `Row<S>` only contains the parent column wrapped
   * in `MaskedValue<T>`.
   *
   * `kind: "none"` is the explicit opt-out — encrypted columns where
   * the creator genuinely wants plaintext-on-read (e.g. background-
   * job-only read paths). PR 2 / PR 3 branch on `kind === "none"` to
   * skip sibling emission and use the P5 decrypt-on-read path.
   */
  mask?: {
    kind: MaskKind;
    classification: Classification;
  };
  /**
   * **P7 PR 1** — typed_id prefix discriminator for `t.id(prefix?)`.
   * Present iff `type === "id"`. The SDK auto-mint pass (PR 3) will
   * use this prefix to call `typed_id::new(prefix)` when the row is
   * inserted without an explicit id. Absent / undefined means the
   * collection name is used as the prefix (PR 3 deferred decision).
   *
   * Wire-format note: this is what makes `t.id("post")` distinguishable
   * from `t.string()` at the runtime DDL emitter (PR 2) and the
   * INSERT auto-populate pass (PR 3). The bare `type: "id"` discriminator
   * is sufficient for the auto-mint candidate detection.
   */
  idPrefix?: string;
  /**
   * **P7 PR 1** — explicit nullability for `t.actor()` columns. Set
   * to `true` by `.nullable()` (Q-SF-I in the proposal: explicit
   * preferred). The default for `t.actor()` is nullable because
   * system-initiated writes (migrations, background jobs) have no
   * actor. Present iff `type === "actor"`.
   */
  actorNullable?: boolean;
  /**
   * **P7 PR 1** — timestamp auto-population modifier set by
   * `.auto_now()` / `.auto_now_on_update()` on a `t.timestamp()` field.
   *
   * - `"now"` — DEFAULT NOW() at INSERT. Used for `created_at`-style
   *   columns. Emitted as `TIMESTAMPTZ NOT NULL DEFAULT NOW()` on PG
   *   (PR 2).
   * - `"now_on_update"` — DEFAULT NOW() at INSERT AND bumped to NOW()
   *   by every UPDATE (the UPDATE builder appends
   *   `<col> = NOW()` to the SET clause — PR 4). Used for
   *   `updated_at`-style columns.
   *
   * Present iff a chain method set it; absent on bare `t.timestamp()`.
   * The chain method refuses any non-timestamp type at SDK time so
   * the discriminator stays well-formed at register-model.
   */
  timestampAuto?: "now" | "now_on_update";
}

/**
 * Fluent builder for a single field definition.
 *
 * Generic params:
 * - `T` — the inferred TS value type (bare primitive or branded id).
 * - `R` — `true` when the field was marked `.required()`; otherwise `false`.
 * - `M` — **P5.5 PR 1** — the mask kind declared via `.mask({...})`,
 *   or the default `"full"` for `t.encrypted()` columns, or
 *   `undefined` for unmasked columns. Surfaces through
 *   `InferFieldDef` so `Row<S>` wraps masked fields in
 *   `MaskedValue<T>` at the type level.
 * - `E` — encryption filter brand: `undefined` for plain fields,
 *   `"randomised"` for un-filterable encrypted fields, or
 *   `"deterministic"` for equality-only encrypted fields. Used by
 *   `Filter<S>` so the type layer matches the runtime fence.
 *
 * `t.string().required().min(3).max(50)` → `TypeBuilder<string, true>`
 * `t.encrypted()` → `TypeBuilder<string, false, "full", "randomised", false>`
 * `t.string().mask({ kind: "email" })` → `TypeBuilder<string, false, "email", undefined, false>`
 * `t.string().required().default("x")` → `TypeBuilder<string, true, undefined, undefined, true>`
 */
export class TypeBuilder<
  T = unknown,
  R extends boolean = false,
  M extends MaskKind | undefined = undefined,
  E extends EncryptionMode | undefined = undefined,
  D extends boolean = false,
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

  private _def: FieldDef;

  constructor(def: FieldDef) {
    this._def = { ...def };
  }

  /** Returns a frozen copy of the field definition. */
  toFieldDef(): Readonly<FieldDef> {
    return Object.freeze({ ...this._def });
  }

  /** Marks the field as required; validation will fail if the field is absent. */
  required(): TypeBuilder<T, true, M, E, D> {
    this._def.required = true;
    return this as unknown as TypeBuilder<T, true, M, E, D>;
  }

  /** Adds a unique index constraint to the field. */
  unique(): this {
    // **P5 PR 2** — randomised + unique is incoherent: randomised mode
    // produces a fresh nonce per write, so the ciphertext for the
    // same plaintext differs across rows, which defeats any
    // ciphertext-equality uniqueness constraint. Deterministic mode
    // CAN enforce uniqueness because identical plaintexts produce
    // identical ciphertexts under the same (collection, column).
    if (this._def.encrypted !== undefined && this._def.encrypted.mode === "randomised") {
      throw Object.assign(
        new Error(
          "t.encrypted({ mode: 'randomised' }).unique(): unique enforcement requires equality on ciphertext, which randomised mode cannot provide. Switch to { mode: 'deterministic' } or drop .unique().",
        ),
        { code: "UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED" as const },
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
  default(val: FieldDefaultValue | (() => FieldDefaultValue)): TypeBuilder<T, R, M, E, true> {
    this._def.default = val;
    return this as unknown as TypeBuilder<T, R, M, E, true>;
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

  /**
   * **P4 PR 3** — mark this field as a source for the per-collection
   * composite full-text-search index. Only valid on `t.string()` fields;
   * called on any other type throws synchronously with code
   * `FTS_ON_NON_STRING`.
   *
   * ```ts
   * const fields = {
   *   title: t.string().required().fts(),
   *   body:  t.string().required().fts(),
   *   lang:  t.string(),  // not searchable
   * };
   * ```
   *
   * All `.fts()`-marked columns on the same collection are folded into a
   * single composite tsvector + GIN index on PG (Q-P4-B). The optional
   * `language` argument selects the tsvector configuration (`"english"`,
   * `"simple"`, …) — defaults to `"english"`; honoured on PG, ignored on
   * SQLite FTS5 (its default tokenizer is language-agnostic Unicode).
   *
   * Search: `await collection.search({ text: "rust async" })` returns
   * rows ordered by relevance with a synthetic `_rank` column.
   */
  fts(language?: string): this {
    if (this._def.type !== "string") {
      throw Object.assign(
        new Error(
          `.fts(): only valid on t.string() fields, got "${this._def.type}"`,
        ),
        { code: "FTS_ON_NON_STRING" as const },
      );
    }
    const lang = language ?? "english";
    if (typeof lang !== "string" || lang.length === 0 || !/^[A-Za-z0-9_]+$/.test(lang)) {
      throw Object.assign(
        new Error(
          `.fts(language): language must be a [A-Za-z0-9_]+ token (e.g. "english", "simple"), got "${String(lang)}"`,
        ),
        { code: "FTS_INVALID_LANGUAGE" as const },
      );
    }
    this._def.fts = true;
    this._def.ftsLanguage = lang;
    return this;
  }

  /**
   * **P5.5 PR 1** — declare a column-level mask. The platform emits
   * a pre-computed sibling `<col>_masked` column (Path B) at CREATE
   * TABLE time (PR 2), routes default reads through that sibling
   * (PR 3), and exposes the parent column as `MaskedValue<T>` on
   * the SDK surface. The unmask round-trip (PR 4) is the only path
   * to plaintext.
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
   *   ssn:      t.encrypted({ mode: "randomised" }),           // → default mask = "full" + "pii"
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
  mask<K extends MaskKind>(opts: { kind: K; classification?: Classification }): TypeBuilder<T, R, K, E, D> {
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
    // `t.ref()` columns must not carry a mask — see Q-P5-I in the
    // sensitive-field-masking proposal. The FK column is an integer
    // typed id; masking it would defeat the JOIN integrity check
    // and the sibling column would itself participate in the FK
    // semantics (incoherent).
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
    return this as unknown as TypeBuilder<T, R, K, E, D>;
  }

  /**
   * **P7 PR 1** — mark the field as nullable. Today this is meaningful
   * only on `t.actor()` (matches Q-SF-I in the proposal: explicit
   * `.nullable()` preferred over implicit). Calling `.nullable()` on
   * any other type sets the `actorNullable` discriminator only when
   * `type === "actor"`; on other types it's a no-op so existing
   * chain ergonomics aren't disturbed (PR 1 foundation only — wider
   * nullability semantics are out of scope).
   *
   * The TS type-side effect (unwrapping non-nullable to nullable) is
   * deferred to a later PR — PR 1 only ships the wire-format discriminator
   * so PR 2's CREATE TABLE can emit `NULL` vs `NOT NULL` correctly.
   */
  nullable(): this {
    if (this._def.type === "actor") {
      this._def.actorNullable = true;
    }
    return this;
  }

  /**
   * **P7 PR 1** — mark a `t.timestamp()` field as auto-populated to
   * `NOW()` at INSERT. PR 2 emits the DDL as `DEFAULT NOW()`; the
   * INSERT auto-populate pass (PR 3) lets the DB DEFAULT fire when
   * the caller omits the column.
   *
   * Refused on non-timestamp types with code `AUTO_NOW_ON_NON_TIMESTAMP`
   * so misuses fail loudly at schema-definition time rather than
   * silently producing wrong DDL. The validator looks at the underlying
   * `type === "date"` because `t.timestamp()` aliases to date today.
   */
  auto_now(): this {
    if (this._def.type !== "date") {
      throw Object.assign(
        new Error(
          `.auto_now(): only valid on t.timestamp() fields, got "${this._def.type}"`,
        ),
        { code: "AUTO_NOW_ON_NON_TIMESTAMP" as const },
      );
    }
    this._def.timestampAuto = "now";
    return this;
  }

  /**
   * **P7 PR 1** — mark a `t.timestamp()` field as auto-populated to
   * `NOW()` at INSERT AND bumped to `NOW()` by every UPDATE. PR 2
   * emits the column as `DEFAULT NOW()`; PR 4 wires the UPDATE
   * builder to append `<col> = NOW()` to every SET clause.
   *
   * Refused on non-timestamp types with code `AUTO_NOW_ON_NON_TIMESTAMP`
   * (shares the code with `.auto_now()` since the misuse class is
   * identical).
   */
  auto_now_on_update(): this {
    if (this._def.type !== "date") {
      throw Object.assign(
        new Error(
          `.auto_now_on_update(): only valid on t.timestamp() fields, got "${this._def.type}"`,
        ),
        { code: "AUTO_NOW_ON_NON_TIMESTAMP" as const },
      );
    }
    this._def.timestampAuto = "now_on_update";
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
   *
   * Only primitive item types are supported today (`string`, `number`,
   * `boolean`, `date`, `json`, `calendarDate`). Passing `t.ref(...)`,
   * `t.object({...})`, `t.union(...)`, `t.literal(...)` or a nested
   * `t.array(...)` throws synchronously with code
   * `INVALID_ARRAY_ITEM` — the previous unchecked cast silently produced
   * malformed `FieldDef`s (e.g. dropping `refTarget` so `validateRefTargets`
   * could not visit array items).
   */
  array<U>(items: TypeBuilder<U, any, any, any, any>): TypeBuilder<U[]> {
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
        { code: "REF_EMPTY_TABLE" as const },
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
  object<S extends Record<string, TypeBuilder<any, any, any, any, any>>>(shape: S): TypeBuilder<InferSchema<S>> {
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
    return new TypeBuilder<InferSchema<S>>({ type: "object", shape: nested });
  },
  /**
   * **P4 PR 2** — vector embedding field. Stored as pgvector's
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
  vector(dims: number, opts?: { metric?: VectorMetric }): TypeBuilder<number[]> {
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
    return new TypeBuilder<number[]>({
      type: "vector",
      vectorDims: dims,
      vectorMetric: metric,
    });
  },
  /**
   * **P4 PR 3** — geographic point field (WGS84, EPSG:4326). Stored as
   * PostGIS's `geography(POINT, 4326)` column on PG; on SQLite (P4 PR 5)
   * a `BLOB` packed `(lat, lng)` × `f64` = 16 bytes.
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
  geoPoint(): TypeBuilder<{ lat: number; lng: number }> {
    return new TypeBuilder<{ lat: number; lng: number }>({ type: "geoPoint" });
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
   * **P5 PR 2** — byte-array wrap for `t.encrypted({ wraps: t.bytes() })`.
   *
   * At the JS layer the field is exchanged as a base64-encoded string;
   * at the DB layer it becomes a BYTEA column (always — bytes-typed
   * columns outside an `encrypted` wrap aren't yet supported in
   * plugin-db). Outside `t.encrypted({ wraps: ... })` a bare
   * `t.bytes()` schema field is an error at register-model time today;
   * this builder exists so the encrypted-wrap argument is well-typed.
   */
  bytes(): TypeBuilder<string> {
    return new TypeBuilder<string>({ type: "bytes" });
  },
  /**
   * **P5 PR 2** — transparent column encryption. Wraps a string /
   * number / bytes field with AEAD encryption at the storage boundary.
   *
   * ```ts
   * const fields = {
   *   ssn:        t.encrypted({ mode: "randomised" }).required(),
   *   apiKey:     t.encrypted({ mode: "deterministic" }).unique(),
   *   payload:    t.encrypted({ wraps: t.bytes() }),
   *   amount:     t.encrypted({ wraps: t.number() }),
   * };
   * ```
   *
   * Modes:
   * - `"randomised"` (default) — per-write fresh nonce; AAD binds
   *   `(collection, column, row_pk)`. Two encrypts of the same plaintext
   *   produce DIFFERENT ciphertext. Defeats the ciphertext-oracle
   *   attack on rows with shared columns. ALL filtering on the column
   *   is refused at the SDK boundary
   *   (`RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE`).
   * - `"deterministic"` — synthetic nonce HMAC-derived from plaintext;
   *   AAD binds `(collection, column)` only. Same plaintext → same
   *   ciphertext, enabling B-tree equality lookups. Only equality
   *   + `$in` filters are accepted; range / regex / LIKE are refused
   *   with `DETERMINISTIC_ENCRYPTED_OP_NOT_SUPPORTED`.
   *
   * Constraints:
   * - `wraps` must be `t.string()` / `t.number()` / `t.bytes()`. Other
   *   types throw with `ENCRYPTED_WRAPS_UNSUPPORTED`.
   * - The combination `mode: "randomised"` + `.unique()` is refused at
   *   schema-definition time with `UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED`
   *   (randomised mode can't enforce uniqueness without equality).
   * - Applying `t.encrypted()` to a `t.ref()` field is refused with
   *   `ENCRYPTED_ON_REF_UNSUPPORTED` — FK columns must remain unencrypted
   *   so the JOIN integrity check works.
   */
  encrypted<
    T extends string | number | Uint8Array = string,
    Mode extends EncryptionMode = "randomised",
  >(
    opts?: EncryptedFieldOpts<Mode>,
  ): TypeBuilder<T, false, "full", Mode, false> {
    const wrapsBuilder = opts?.wraps;
    let wrapsKind: "string" | "number" | "bytes" = "string";
    if (wrapsBuilder !== undefined) {
      if (!(wrapsBuilder instanceof TypeBuilder)) {
        throw Object.assign(
          new Error("t.encrypted({ wraps }): wraps must be a TypeBuilder (t.string() / t.number() / t.bytes())"),
          { code: "ENCRYPTED_WRAPS_UNSUPPORTED" as const },
        );
      }
      const def = wrapsBuilder.toFieldDef();
      if (def.type === "string") wrapsKind = "string";
      else if (def.type === "number") wrapsKind = "number";
      else if (def.type === "bytes") wrapsKind = "bytes";
      else {
        throw Object.assign(
          new Error(
            `t.encrypted({ wraps }): only string / number / bytes are supported, got "${def.type}"`,
          ),
          { code: "ENCRYPTED_WRAPS_UNSUPPORTED" as const },
        );
      }
    }
    const mode = (opts?.mode ?? "randomised") as Mode;
    if (mode !== "randomised" && mode !== "deterministic") {
      throw Object.assign(
        new Error(
          `t.encrypted({ mode }): must be "randomised" or "deterministic", got "${String(mode)}"`,
        ),
        { code: "ENCRYPTED_INVALID_MODE" as const },
      );
    }
    const keyId = opts?.keyId ?? "default";
    if (typeof keyId !== "string" || keyId.length === 0 || !/^[A-Za-z0-9_]+$/.test(keyId)) {
      throw Object.assign(
        new Error(
          `t.encrypted({ keyId }): keyId must be a [A-Za-z0-9_]+ token, got "${String(keyId)}"`,
        ),
        { code: "ENCRYPTED_INVALID_KEY_ID" as const },
      );
    }
    // The DB column TYPE is BYTEA — the encryption pass + DDL emitter
    // (`field_to_column` in plugin-db) ignore the `type` field when
    // `encrypted` is present. We still carry the wrapped primitive's
    // type so validators see the right user-facing shape (e.g.
    // `validate.ts` rejects `123` for a wraps=string column).
    //
    // **P5.5 PR 1** — fail-safe default-mask rule (§3 of the
    // sensitive-field-masking proposal): every `t.encrypted()` column
    // gets `mask: { kind: "full", classification: "pii" }` at
    // builder time when no `.mask(...)` is chained. The
    // intentional path to plaintext-on-read is the explicit
    // `.mask({ kind: "none" })` opt-out. Chaining `.mask({...})`
    // after `t.encrypted()` overwrites this default via the
    // builder's `.mask` method (assigns `_def.mask` unconditionally).
    return new TypeBuilder<T, false, "full", Mode>({
      type: wrapsKind === "bytes" ? "bytes" : wrapsKind === "number" ? "number" : "string",
      encrypted: { mode, keyId, wraps: wrapsKind },
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
  /**
   * **P7 PR 1** — typed_id field. At the JS layer the field is exchanged
   * as a string carrying the UUIDv7 + base62 + optional entity prefix
   * (e.g. `"post_01HXYZ..."`). At the DB layer it's a TEXT column.
   *
   * The optional `prefix` argument names the entity tag the SDK
   * auto-mint pass (PR 3) will pass to `typed_id::new(prefix)` on
   * inserts that omit `id`. Omitting `prefix` defers the choice to
   * PR 3 (default-to-collection-name).
   *
   * Wire shape: `{ type: "id", idPrefix?: string }`. The `type: "id"`
   * discriminator is the signal the PR 3 auto-populate pass uses to
   * find the auto-mint candidate without keying on the literal name
   * `"id"` — so a model could in principle have a non-`id`-named
   * primary key (though §2.1 of the proposal pins the seven names).
   *
   * PR 1 ships the builder + wire discriminator only. Auto-mint
   * behaviour lands in PR 3; CREATE TABLE emission lands in PR 2.
   */
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
    }
    const def: FieldDef = { type: "id" };
    if (prefix !== undefined) def.idPrefix = prefix;
    return new TypeBuilder<string>(def);
  },
  /**
   * **P7 PR 1** — actor field. Stores a typed_id at the DB layer
   * (TEXT) sourced from the current request's `SessionMinter.actor_id`
   * (P3). Used for `created_by` / `updated_by` system fields, and
   * available to creators who want their own actor-tracking columns
   * (e.g. `last_edited_by`).
   *
   * Nullable by convention (matches Q-SF-I in the proposal: explicit
   * `.nullable()` is the canonical declaration form, but the default
   * is nullable because system-initiated writes have no actor). The
   * default is captured by `actorNullable = true` so the wire shape
   * is unambiguous regardless of whether `.nullable()` was chained.
   *
   * PR 1 ships the builder + wire discriminator only. PR 3 wires the
   * INSERT auto-populate from `SessionMinter.actor_id`; PR 4 wires
   * the UPDATE-time bump.
   */
  actor(): TypeBuilder<string | null> {
    // Default-nullable: written explicitly so the wire shape is
    // unambiguous. `.nullable()` is a no-op (already true) for the
    // explicit form callers may prefer.
    return new TypeBuilder<string | null>({ type: "actor", actorNullable: true });
  },
  union<V extends readonly TypeBuilder<any, any, any, any, any>[]>(...variants: V): TypeBuilder<InferUnion<V>> {
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
   * `VERSION_MISMATCH` error).
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
   * declared index. Auto-generated columns (`id`, `created_at`,
   * `updated_at`, `created_by`, `updated_by`, `deleted_at`, `version`)
   * are also accepted alongside user fields.
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

  /**
   * The set of field names this schema accepts in `.index(...)`. Includes
   * user-declared fields plus the auto-generated system columns the
   * collection always carries. With the SDK's snake_case system-field
   * contract, these names match the underlying columns 1:1.
   */
  private _knownFieldNames(): Set<string> {
    const out = new Set<string>([
      "id",
      "created_at",
      "updated_at",
      "created_by",
      "updated_by",
      "version",
      "deleted_at",
    ]);
    const f = this.fields;
    if (f !== null && typeof f === "object") {
      for (const k of Object.keys(f as Record<string, unknown>)) out.add(k);
    }
    return out;
  }

  /** Enable soft delete — deleteOne/deleteMany set `deleted_at` instead of removing rows. */
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
   * `{ data: null, error: { code: "VERSION_MISMATCH" } }`.
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
