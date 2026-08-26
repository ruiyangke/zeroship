// Minimal, self-contained db type-builder surface (re-authored INSIDE
// `zero-migrate`).
//
// SCOPE: this is the exact subset `zero-migrate`'s `db-lexicon` bridge
// (`colTypeFromDbField` / `fromDb`) depends on — the `FieldDef` schema-field shape
// and the fluent `TypeBuilder` (`t.*` factories return one; `.toFieldDef()` reduces
// it to a `FieldDef`). It was previously a separate workspace db package; it is
// inlined here so the standalone migrate package carries no external db
// dependency. It is NOT a full ORM surface (query, CRUD, aggregation, generated
// typing, validation) — only the FK/column-type bridge inputs.
//
// It is a REAL implementation (not an empty placeholder): the `FieldDef` union
// carries the exact `type` discriminants + facet fields (`encrypted`, `refTarget`,
// `vectorDims`, `required`, `unique`) the migrate bridge maps, and `TypeBuilder` is
// a genuine fluent class (`.required()`, `.optional()`, `.unique()`, `.toFieldDef()`)
// so the bridge's `instanceof TypeBuilder` check, its `.toFieldDef()` reduce, and its
// exhaustive `FieldDef["type"]` switch all type-check and run.

/** The canonical db field type discriminant (`TypeName`). The migrate `db-lexicon`
 *  bridge enumerates every storage-backed member; the non-storage members
 *  (`object`/`union`/`literal`/`array`/`actor`/`calendarDate`) reduce to a hard
 *  `UnsupportedColTypeError` there. */
export type TypeName =
  | "string"
  | "number"
  | "boolean"
  | "date"
  | "json"
  | "bytes"
  | "geoPoint"
  | "id"
  | "ref"
  | "vector"
  | "object"
  | "union"
  | "literal"
  | "array"
  | "actor"
  | "calendarDate";

/**
 * The normalized descriptor a `TypeBuilder` reduces to. The `type` discriminant is
 * the load-bearing field the migrate bridge switches on; the optional facet fields
 * carry the extra data specific facets need:
 *   - `encrypted` — an encryption facet wrapping the inner primitive `type`;
 *   - `refTarget` — the target table name for a `ref` (FK) field;
 *   - `vectorDims` — the declared dimensionality for a `vector` field;
 *   - `required`/`unique` — nullability + uniqueness carried onto the migration column.
 */
export interface FieldDef {
  type: TypeName;
  /** Column is nullable (no `NOT NULL`). A `required()` field clears this. */
  optional?: boolean;
  /** Column is non-nullable — the migrate `fromDb` bridge reads this flag to carry
   *  `.required()` over to the migration column's `.notNull()`. */
  required?: boolean;
  /** Column carries a UNIQUE constraint. */
  unique?: boolean;
  /** Present (the inner primitive `FieldDef`) when this is an encrypted wrapper. */
  encrypted?: FieldDef;
  /** The referenced table name for a `ref` (foreign-key) field. */
  refTarget?: string;
  /** The declared dimensionality for a `vector` (pgvector) field. */
  vectorDims?: number;
}

/**
 * The fluent column-type builder a `t.*` factory returns. It carries an in-progress
 * {@link FieldDef} and reduces to it via {@link toFieldDef}. The generic parameters
 * are state phantoms; only the fluent facets + `toFieldDef()` are load-bearing for
 * the migrate bridge.
 */
export class TypeBuilder<
  _TS = unknown,
  _TO = unknown,
  _TD = unknown,
  _TR = unknown,
  _TE = unknown,
> {
  readonly #def: FieldDef;

  constructor(def: FieldDef) {
    this.#def = def;
  }

  /** Mark the column nullable. */
  optional(): TypeBuilder<_TS, _TO, _TD, _TR, _TE> {
    return new TypeBuilder({ ...this.#def, optional: true });
  }

  /** Mark the column non-nullable (clears any inherited `optional`). Sets the
   *  `required` flag the migrate `fromDb` bridge reads for `.notNull()`. */
  required(): TypeBuilder<_TS, _TO, _TD, _TR, _TE> {
    return new TypeBuilder({ ...this.#def, optional: false, required: true });
  }

  /** Add a UNIQUE constraint. */
  unique(): TypeBuilder<_TS, _TO, _TD, _TR, _TE> {
    return new TypeBuilder({ ...this.#def, unique: true });
  }

  /** Reduce this builder to its normalized {@link FieldDef}. */
  toFieldDef(): FieldDef {
    return { ...this.#def };
  }
}

/** Options for {@link t.encrypted}. */
export interface EncryptedOptions {
  /** The inner primitive builder the encryption wraps. Defaults to `t.string()`. */
  wraps?: TypeBuilder;
}

/** The `t.*` factory lexicon — the storage-backed subset the migrate bridge lifts,
 *  plus the non-storage shapes it rejects (so the bridge's hard-boundary test can
 *  construct them). Each factory returns a {@link TypeBuilder} whose `.toFieldDef()`
 *  yields the canonical {@link FieldDef}. */
export const t = {
  // Storage-backed scalars.
  string: () => new TypeBuilder({ type: "string" }),
  number: () => new TypeBuilder({ type: "number" }),
  boolean: () => new TypeBuilder({ type: "boolean" }),
  /** `date`/`timestamp` — the db date discriminant is `"date"`; `timestamp` is an
   *  alias factory producing the same `FieldDef.type`. */
  date: () => new TypeBuilder({ type: "date" }),
  timestamp: () => new TypeBuilder({ type: "date" }),
  json: () => new TypeBuilder({ type: "json" }),
  bytes: () => new TypeBuilder({ type: "bytes" }),
  geoPoint: () => new TypeBuilder({ type: "geoPoint" }),
  id: (_prefix?: string) => new TypeBuilder({ type: "id" }),
  ref: (target: string) => new TypeBuilder({ type: "ref", refTarget: target }),
  vector: (dims: number) => new TypeBuilder({ type: "vector", vectorDims: dims }),
  /** An encrypted wrapper over an inner primitive (default `string`). The bridge
   *  keeps the wrapped primitive in `type` and carries the encryption facet. */
  encrypted: (opts: EncryptedOptions = {}) => {
    const inner = (opts.wraps ?? t.string()).toFieldDef();
    return new TypeBuilder({ type: inner.type, encrypted: inner });
  },
  // Non-storage / type-only shapes — the migrate bridge rejects these with a hard
  // `UnsupportedColTypeError`; provided so tests can construct the boundary cases.
  object: (_shape?: Record<string, TypeBuilder>) => new TypeBuilder({ type: "object" }),
  union: (..._members: TypeBuilder[]) => new TypeBuilder({ type: "union" }),
  literal: (_value?: unknown) => new TypeBuilder({ type: "literal" }),
  array: (_of?: TypeBuilder) => new TypeBuilder({ type: "array" }),
  actor: () => new TypeBuilder({ type: "actor" }),
  calendarDate: () => new TypeBuilder({ type: "calendarDate" }),
} as const;
