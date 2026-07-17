// `zero-migrate` — the SHARED column-type lexicon bridge from the db type-builder
// surface (`./db-types.js`).
//
// The migration DSL and the runtime schema share the same primitive type
// lexicon. A `t.text()` written in a migration is the same dialect-neutral
// `ColType` the db schema reduces to. References are intentionally asymmetric:
// the runtime schema retains its legacy `dbType.ref("users")` FieldDef, while a
// migration records an explicit primitive type plus `IrColumn.references`.
//
// We REUSE the db type machinery rather than duplicate the lexicon: the db `t.*`
// factories return a `TypeBuilder` whose `.toFieldDef()` yields a `FieldDef`
// carrying the canonical `TypeName` discriminant (`"string"`, `"ref"`,
// `"vector"`, …). This module is the SINGLE source for the bridge from the db
// `FieldDef.type` (`TypeName`) space INTO the migrate `ColType` space. There is
// exactly one mapping, defined once here.
//
// NOTE: this is NOT the inverse of the engine's Rust `col_type_to_token`
// (`crates/zero-migrate/src/render/lower.rs`). That function emits engine-internal
// masked-sibling descriptor tokens (`"int"` for `Int|BigInt`, `"number"` for
// `Float|Decimal`, `"string"` for `Uuid|Text`, …) — a different, overlapping
// token set from the db `FieldDef` discriminants this bridge consumes (the db
// `FieldDef` union emits `"number"`/`"id"`/… and never emits `"int"`). The two
// are NOT round-trip inverses; the only invariant they share is that migrate
// `ColType` is generated from the engine IR schema. Its legacy `ref` arm remains
// the bridge carrier for runtime db schemas; the public migration `t.ref()`
// factory is removed and typed migration references do not use this arm.
//
// BINDING: this bridge converts a column's TYPE only. It never binds
// table/column NAMES to the live schema — a runtime `dbType.ref(target)` carries
// the target table as a plain string. Migration `.references(table, column)` is
// recorded separately on `IrColumn` and validated by the migration planner.

import { TypeBuilder, type FieldDef } from "./db-types.js";

import type { ColType } from "./types.js";

/** The canonical db `FieldDef.type` discriminant. Reusing the db field-def shape
 *  keeps the lexicon single-source: we read the db type token, we do not re-spell
 *  it. */
export type DbFieldType = FieldDef["type"];

/** A db schema field: either the fluent `TypeBuilder` a `t.*` factory
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
      `db field type "${dbType}" has no dialect-neutral migration ColType; ` +
        `model it explicitly (e.g. a migration json column or a separate table ` +
        `plus an explicitly typed .references())`,
    );
    this.dbType = dbType;
  }
}

/** Reduce a db schema field to its `FieldDef` form (identity for a raw
 *  `FieldDef`; `.toFieldDef()` for a `TypeBuilder`). */
function toFieldDef(field: DbSchemaField): FieldDef {
  if (field instanceof TypeBuilder) return field.toFieldDef();
  if (field && typeof field === "object" && typeof (field as FieldDef).type === "string") {
    return field as FieldDef;
  }
  throw new TypeError(
    "colTypeFromDbField(field): expected a db TypeBuilder (t.*) or a FieldDef",
  );
}

/**
 * Map a db schema field to the dialect-neutral migration {@link ColType}.
 *
 * This is the ONE place the db `FieldDef.type` (`TypeName`) space is bridged into
 * the migration `ColType` space. It is the proof the two surfaces share one
 * lexicon: a `dbType.ref("users")` from the db type builder yields the legacy
 * `{ ref: { references: "users" } }` bridge carrier. Typed migration references
 * instead retain an explicit local `ColType` and add `IrColumn.references`.
 * (This is a one-way bridge over the db type space, NOT the inverse of the engine's `col_type_to_token`, whose
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
    case "boolean":
      return "boolean";
    case "date":
      return "timestamp";
    case "json":
      return "json";
    case "bytes":
      return "bytes";
    case "geoPoint":
      return "geoPoint";
    // `dbType.id(...)` is the legacy internal platform ID field. The runtime mints
    // `<prefix>_<22 base62 UUIDv7>` values; this is neither TypeID nor a public
    // migration-column shortcut. Its historical bridge carrier is neutral `uuid`.
    case "id":
      return "uuid";
    // A foreign-key column: the neutral `ref` arm carries the target table as a
    // PLAIN STRING (never live-schema-bound). `refTarget` is required on a
    // well-formed `dbType.ref(...)` FieldDef.
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
    // db `FieldDef.type` space, so they must be enumerated explicitly
    // — the `default` arm below is the exhaustiveness guard, not a catch-all.
    case "object":
    case "union":
    case "literal":
    case "array":
    case "actor":
    case "calendarDate":
      throw new UnsupportedColTypeError(def.type);
    default: {
      // Exhaustiveness guard: every member of `DbFieldType` (= the db `TypeName`
      // single source) must be handled by an arm above. If the db type builder
      // adds a NEW storage-backed `TypeName`, `def.type` is no
      // longer `never` here and THIS LINE FAILS tsc — turning silent bridge
      // drift into a build error instead of a fail-closed runtime surprise. The
      // throw remains for the untyped/at-runtime path (a hand-built FieldDef
      // carrying an out-of-union token).
      const _exhaustive: never = def.type;
      throw new UnsupportedColTypeError(_exhaustive);
    }
  }
}
