// `@zeroship/migrate` — the SHARED column-type lexicon bridge from `@zeroship/db`
// (§3.2 / §3.3 / PR5).
//
// PR5 goal (A): the migration DSL and the runtime schema share ONE type lexicon.
// A `t.text()` written in a migration is the same dialect-neutral `ColType` the
// `@zeroship/db` schema reduces to, so a `t.ref("users")` FK declared in a live
// `@zeroship/db` schema lowers to the IDENTICAL `{ ref: { references: "users" } }`
// `ColType` an `addColumn("…", "…", t.ref("users"))` migration column produces.
//
// We REUSE the `@zeroship/db` type machinery rather than duplicate the lexicon:
// the db `t.*` factories return a `TypeBuilder` whose `.toFieldDef()` yields a
// `FieldDef` carrying the canonical `TypeName` discriminant (`"string"`,
// `"ref"`, `"vector"`, …). This module is the SINGLE source for the bridge from
// the `@zeroship/db` `FieldDef.type` (`TypeName`) space INTO the migrate
// `ColType` space. There is exactly one mapping, defined once here.
//
// NOTE: this is NOT the inverse of the engine's Rust `col_type_to_token`
// (`third_party/zero-migrate/crates/zero-migrate/src/render/lower.rs`). That
// function emits engine-internal masked-sibling descriptor tokens
// (`"int"` for `Int|BigInt`,
// `"number"` for `Float|Decimal`, `"string"` for `Uuid|Text`, …) — a different,
// overlapping token set from the `@zeroship/db` `FieldDef` discriminants this
// bridge consumes (the db `FieldDef` union emits `"number"`/`"id"`/… and never
// emits `"int"`). The two are NOT round-trip inverses; the only invariant they
// share is that migrate `ColType` is generated from the engine IR schema, so a
// db `t.ref("users")` and a migrate `t.ref("users")` reduce to the byte-
// identical `{ ref: { references: "users" } }` ColType.
//
// BINDING (§3.3): this bridge converts a column's TYPE only. It never binds
// table/column NAMES to the live schema — a `t.ref(target)` carries the target
// table as a plain string (existence validated at apply time), exactly as the
// migration DSL's own `t.ref` does. §3.3 constrains WHEN a name is resolved, not
// HOW MANY names a construct carries: the migration DSL's column-level
// `.references(table, column)` facet is just as unbound and records both halves
// of the target. It is a separate construct from this `ref` TYPE arm and is
// recorded on `IrColumn.references`, not here.

import { TypeBuilder, type FieldDef } from "@zeroship/db";

import type { ColType } from "./types.js";

/** The canonical `@zeroship/db` `FieldDef.type` discriminant. Reusing the db
 *  field-def shape keeps the lexicon single-source: we read the db type token,
 *  we do not re-spell it. */
export type DbFieldType = FieldDef["type"];

/** A `@zeroship/db` schema field: either the fluent `TypeBuilder` a `t.*` factory
 *  returns, or the already-normalized `FieldDef` it reduces to. Both reduce to
 *  the same `ColType` through {@link colTypeFromDbField}. */
export type DbSchemaField = TypeBuilder<any, any, any, any, any> | FieldDef;

/** A column type that has no portable dialect-neutral `ColType` (e.g. a JSON
 *  `array`, a nested `object`, a discriminated `union`, a `literal`). Mirrors the
 *  engine's hard structured boundary: a non-expressible type is a hard error, not
 *  a silent fallback (property A). */
export class UnsupportedColTypeError extends Error {
  readonly code = "COLTYPE_UNSUPPORTED" as const;
  readonly dbType: string;
  constructor(dbType: string) {
    super(
      `@zeroship/db field type "${dbType}" has no dialect-neutral migration ColType; ` +
        `model it explicitly (e.g. a json column or a separate collection + t.ref)`,
    );
    this.dbType = dbType;
  }
}

/** Reduce a `@zeroship/db` `FieldDef` to its `FieldDef` form (identity for a raw
 *  `FieldDef`; `.toFieldDef()` for a `TypeBuilder`). */
function toFieldDef(field: DbSchemaField): FieldDef {
  if (field instanceof TypeBuilder) return field.toFieldDef();
  if (field && typeof field === "object" && typeof (field as FieldDef).type === "string") {
    return field as FieldDef;
  }
  throw new TypeError(
    "colTypeFromDbField(field): expected a @zeroship/db TypeBuilder (t.*) or a FieldDef",
  );
}

/**
 * Map a `@zeroship/db` schema field to the dialect-neutral migration {@link ColType}.
 *
 * This is the ONE place the db `FieldDef.type` (`TypeName`) space is bridged into
 * the migration `ColType` space. It is the proof the two surfaces share one
 * lexicon: a `t.ref("users")` from `@zeroship/db` yields
 * `{ ref: { references: "users" } }`, byte-identical to the migration DSL's own
 * `t.ref("users")._type` (verified by test). (This is a one-way bridge over the
 * db type space, NOT the inverse of the engine's `col_type_to_token`, whose
 * `"int"`/`"number"` outputs are engine-internal descriptors — see the module
 * header.)
 *
 * Type-only / non-storage db field shapes (`object`/`union`/`literal`/`array`/
 * `actor`/`calendarDate`) that have no single portable column type throw
 * {@link UnsupportedColTypeError} — a hard structured boundary, never a silent
 * fallback.
 */
export function colTypeFromDbField(field: DbSchemaField): ColType {
  const def = toFieldDef(field);
  // An encrypted column wraps an inner primitive (`string`/`number`/`bytes`); the
  // db `FieldDef` keeps the wrapped primitive in `type` and carries the encryption
  // facet alongside. Reduce to the neutral `encrypted` ColType whose `of` recurses
  // on the inner token — the same shape the engine's `ColType::Encrypted { of }`
  // carries. Checked before the type switch so the facet drives the arm.
  if (def.encrypted !== undefined) {
    const inner = colTypeFromDbField({ type: def.type } as FieldDef);
    return { encrypted: { of: inner } };
  }
  switch (def.type) {
    // Scalars whose db token maps 1:1 onto a neutral ColType.
    case "string":
      return "string";
    case "number":
      return "double";
    // The generator emits these when it reproduces the columns the migrations
    // built, so they reach this bridge even though nobody authors them:
    // `t.int()` lands in the descriptor as `"int"`, not `"number"`. Each maps
    // onto the neutral ColType of the same name, so the round trip is exact
    // rather than widened -- mapping `int` to `double` here would quietly
    // restate an integer column as a float one.
    case "int":
    case "integer":
      return "int";
    case "bigInt":
      return "bigInt";
    case "float":
      return "double";
    case "boolean":
      return "boolean";
    case "date":
    // `timestamp` reaches here for the same reason, and `date` already maps to
    // the `timestamp` ColType, so they share an arm.
    case "timestamp":
      return "timestamp";
    case "json":
      return "json";
    case "bytes":
      return "bytes";
    case "geoPoint":
      return "geoPoint";
    // `t.id(...)` is a typed_id stored as a uuid/text PK candidate (the runtime
    // mints `<prefix>_<base62>`); it reduces to the neutral `uuid` ColType, the
    // same column the migration DSL's `t.id()` carries.
    case "id":
      return "uuid";
    // A foreign-key column: the neutral `ref` arm carries the target TABLE only,
    // because that is all a `@zeroship/db` `t.ref(...)` FieldDef holds — its
    // `refTarget` is a single table name, with the referenced column implied by
    // the target's primary key. That is a property of the SOURCE shape, not of
    // §3.3: §3.3 is a TYPING stance (names stay plain strings, validated at apply
    // time, never bound to the live schema at tsc time) and says nothing about how
    // many names a construct may carry — the migration DSL's own
    // `.references(table, column)` facet is equally unbound and carries BOTH.
    // A migration that needs the target column records the column-level
    // `references` facet (`IrColumn.references`) beside an explicit type; it does
    // not come through this arm. `refTarget` is required on a well-formed
    // `t.ref(...)` FieldDef.
    case "ref": {
      const references = def.refTarget;
      if (typeof references !== "string" || references.length === 0) {
        throw new TypeError("colTypeFromDbField: a ref field must carry a refTarget table name");
      }
      return { ref: { references } };
    }
    // A pgvector column carries its declared dimensionality.
    case "vector": {
      const dims = def.vectorDims;
      if (typeof dims !== "number" || !Number.isInteger(dims) || dims <= 0) {
        throw new TypeError("colTypeFromDbField: a vector field must carry positive integer vectorDims");
      }
      return { vector: { vector: dims } };
    }
    // Non-storage / type-only db field shapes that have no single portable
    // column type are a hard structured boundary (property A): they reduce to
    // `UnsupportedColTypeError`, never a silent fallback. These ARE part of the
    // `@zeroship/db` `FieldDef.type` space, so they must be enumerated explicitly
    // — the `default` arm below is the exhaustiveness guard, not a catch-all.
    case "object":
    case "union":
    case "literal":
    case "array":
    case "actor":
    case "calendarDate":
      throw new UnsupportedColTypeError(def.type);
    default: {
      // Exhaustiveness guard: every member of `DbFieldType` (= the @zeroship/db
      // `TypeName` single source) must be handled by an arm above. If
      // @zeroship/db adds a NEW storage-backed `TypeName`, `def.type` is no
      // longer `never` here and THIS LINE FAILS tsc — turning silent bridge
      // drift into a build error instead of a fail-closed runtime surprise. The
      // throw remains for the untyped/at-runtime path (a hand-built FieldDef
      // carrying an out-of-union token).
      const _exhaustive: never = def.type;
      throw new UnsupportedColTypeError(_exhaustive);
    }
  }
}
