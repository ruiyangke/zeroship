/**
 * `installSchema` — framework-internal helper behind the runtime schema
 * descriptor install path. Stage 7 of the refactor moved
 * this out of `@zeroship/db` into `@zeroship/bootstrap` so the same
 * implementation backs both the runtime crate's bootstrap and the Vite
 * plugin's dev path. User code MUST NOT call this.
 *
 * Behaviour:
 *   - Plants typed `Collection` wrappers PLUS the `transaction` / `live`
 *     extension methods as own properties on the supplied `env` (the
 *     native `ZeroshipDb` handle — `env.db` in production, a mock in
 *     tests). After this returns, `env.<collection>.find(...)` and
 *     `env.transaction(tx => ...)` are live.
 *   - Returns `{ collections }`. Runtime descriptor entries are planted
 *     natively before this JavaScript installer runs.
 *
 * Re-entrancy: a second call with overlapping names re-installs the
 * Collection wrappers (`configurable: true` on the descriptors).
 * Reserved native names throw at boot rather than silently shadowing.
 */
import {
  drainCollectionLoaders,
  enterTransactionScope,
  exitTransactionScope,
  captureNativeTransaction,
  Collection,
  type NativeDb,
  type NativeTransactionFn,
  Query,
  createLive,
  type LiveOptions,
  type LiveQuery,
  naming,
  SchemaBuilder,
  TypeBuilder,
  ok,
  err,
  type NamingStrategy,
  type NamedIndexSpec,
  type Result,
  type Row,
  type RowInput,
  type UpsertOptions,
  type UpdateExpression,
  type Filter,
  type IsolationLevel,
  type WithSpec,
  type WithRelations,
  type PlainObject,
  type FieldDef,
  CONFINED_SYSTEM_SHAPE_COLUMN_NAMES,
  CONFINED_SYSTEM_SHAPE_ASSIGNMENTS,
} from "@zeroship/db/internal";

// ---------------------------------------------------------------------------
// normalizeSchema + expandUnionToFlatColumns (moved from @zeroship/db/schema)
// ---------------------------------------------------------------------------

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/**
 * **Migration-first cutover (P5 S3)** — the bundled runtime schema source.
 *
 * The migration fold emits `schema.runtime.json` and the runtime carries it in
 * `manifest.runtime_descriptor`. v2 is `{ version: 2, collections: { ... } }`:
 * each collection carries already-resolved wire `FieldDef`s (snake_case
 * columns, system fields included), runtime options, and plain named indexes.
 * v2 is the sole runtime schema source.
 *
 * **v2 over v1 because the guarantee changed, not because the shape grew.** Every
 * `FieldDef` in a v2 descriptor carries a `storage` block naming the physical column
 * a default projection reads and, when they differ, the column holding the
 * authoritative value. A consumer that stops deriving that second name by string
 * formatting depends on it being true of every field it is handed, and a committed v1
 * artifact does not carry it. Refusing a non-v2 descriptor outright is the whole reason the
 * number moved.
 */
type RuntimeStrictness = "strict" | "lenient" | "off";
type RuntimeCollectionDescriptorV2 = {
  fields: Record<string, FieldDef>;
  options: {
    softDelete: boolean;
    versioning: boolean;
    strictness?: RuntimeStrictness;
  };
  indexes: readonly NamedIndexSpec[];
};
export type RuntimeSchemaDescriptor = {
  version: 2;
  collections: Record<string, RuntimeCollectionDescriptorV2>;
};

function assertRuntimeDescriptorV2(
  descriptor: RuntimeSchemaDescriptor | undefined,
): { version: 2; collections: Record<string, RuntimeCollectionDescriptorV2> } | null {
  if (descriptor === undefined) return null;
  if (
    descriptor === null ||
    typeof descriptor !== "object" ||
    (descriptor as { version?: unknown }).version !== 2 ||
    typeof (descriptor as { collections?: unknown }).collections !== "object" ||
    (descriptor as { collections?: unknown }).collections === null
  ) {
    throw Object.assign(
      new Error(
        "@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: expected v2 object with { version: 2, collections }",
      ),
      { code: "INVALID_RUNTIME_DESCRIPTOR" as const },
    );
  }

  const collections = (descriptor as { collections: Record<string, unknown> }).collections;
  for (const [name, raw] of Object.entries(collections)) {
    if (raw === null || typeof raw !== "object") {
      throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} must be an object`);
    }
    const collection = raw as Record<string, unknown>;
    if (collection.fields === null || typeof collection.fields !== "object" || Array.isArray(collection.fields)) {
      throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} requires object field "fields"`);
    }
    for (const [fieldName, field] of Object.entries(collection.fields as Record<string, unknown>)) {
      if (
        field === null ||
        typeof field !== "object" ||
        typeof (field as { type?: unknown }).type !== "string"
      ) {
        throw invalidRuntimeDescriptor(
          `collection ${JSON.stringify(name)} field ${JSON.stringify(fieldName)} requires object FieldDef with string "type"`,
        );
      }
    }
    if (collection.options === null || typeof collection.options !== "object" || Array.isArray(collection.options)) {
      throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} requires object field "options"`);
    }
    const options = collection.options as Record<string, unknown>;
    if (typeof options.softDelete !== "boolean" || typeof options.versioning !== "boolean") {
      throw invalidRuntimeDescriptor(
        `collection ${JSON.stringify(name)} options requires boolean "softDelete" and "versioning"`,
      );
    }
    if (
      options.strictness !== undefined &&
      options.strictness !== "strict" &&
      options.strictness !== "lenient" &&
      options.strictness !== "off"
    ) {
      throw invalidRuntimeDescriptor(
        `collection ${JSON.stringify(name)} options.strictness must be "strict", "lenient", or "off"`,
      );
    }
    if (!Array.isArray(collection.indexes)) {
      throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} requires array field "indexes"`);
    }
    for (const [i, index] of collection.indexes.entries()) {
      if (index === null || typeof index !== "object") {
        throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} indexes[${i}] must be an object`);
      }
      const idx = index as Record<string, unknown>;
      if (typeof idx.name !== "string" || !Array.isArray(idx.fields) || !idx.fields.every((f) => typeof f === "string")) {
        throw invalidRuntimeDescriptor(
          `collection ${JSON.stringify(name)} indexes[${i}] requires string "name" and string[] "fields"`,
        );
      }
      if (idx.unique !== undefined && typeof idx.unique !== "boolean") {
        throw invalidRuntimeDescriptor(`collection ${JSON.stringify(name)} indexes[${i}].unique must be boolean`);
      }
    }
  }

  return descriptor as { version: 2; collections: Record<string, RuntimeCollectionDescriptorV2> };
}

function invalidRuntimeDescriptor(detail: string): Error & { code: "INVALID_RUNTIME_DESCRIPTOR" } {
  return Object.assign(
    new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: ${detail}`),
    { code: "INVALID_RUNTIME_DESCRIPTOR" as const },
  );
}

function runtimeDescriptorFields(
  descriptor: RuntimeSchemaDescriptor | undefined,
): Record<string, Record<string, FieldDef>> | null {
  const v2 = assertRuntimeDescriptorV2(descriptor);
  if (v2 !== null) {
    const out: Record<string, Record<string, FieldDef>> = {};
    for (const [name, collection] of Object.entries(v2.collections)) {
      out[name] = collection.fields;
    }
    return out;
  }
  return null;
}

/** Input form: a record of TypeBuilder instances. Field values must be
 *  produced by the `t.*` API (`t.string()`, `t.number()`, etc.).
 *
 *  `TypeBuilder`'s 5th param (`D`, the has-default brand set by
 *  `.default(...)`) must be `any` here, not omitted - omitting it pins the
 *  default value `false`, which rejects every defaulted field
 *  (`t.string().default("x")` has `D=true`) at the type layer even though
 *  it is a completely valid, common schema declaration at runtime. Ticket
 *  #267 surfaced this: `normalizeSchema({ role: t.string().default("x") })`
 *  had never been typechecked (this package's own tests run through tsx,
 *  which strips types) and failed the moment `@zeroship/db`'s test suite
 *  finally ran `tsc --noEmit` over a call site that used it. */
type SchemaFieldRecord = Record<string, TypeBuilder<unknown, boolean, any, any, any>>;

/** Accepts either a record of fields OR a top-level union TypeBuilder. */
type SchemaInputOrUnion = SchemaFieldRecord | TypeBuilder<unknown, boolean, any, any, any>;

function isTypeBuilder(value: unknown): value is TypeBuilder<unknown, boolean, any, any, any> {
  return value instanceof TypeBuilder;
}

function isSchemaBuilder(value: unknown): value is SchemaBuilder<Record<string, unknown>> {
  return value instanceof SchemaBuilder;
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object";
}

function isFieldDef(value: unknown): value is FieldDef {
  return isPlainRecord(value) && typeof value.type === "string";
}

/**
 * The platform-managed column names, taken from the operator charter
 * (`policies/confined-system-shape.inject.toml`) via its generated projection.
 * Creator schemas cannot declare fields with these names; fencing at
 * schema-declaration time surfaces the failure in `pnpm dev` rather than after
 * a worker round-trip.
 *
 * **This used to be seven string literals restating the charter**, with a
 * doc-comment asking the reader to keep them in step with the Rust side by
 * hand. It is now derived, so an eighth platform column is a charter line and
 * `tests/inject_policy_mirror_gate.sh` fails if the projection goes stale
 * against the fragment. The hand-sync instruction is gone because there is
 * nothing left to sync.
 */
const SYSTEM_FIELD_NAMES: readonly string[] = CONFINED_SYSTEM_SHAPE_COLUMN_NAMES;

/**
 * Copy a descriptor-supplied `FieldDef`, stamping the charter's assignment onto
 * it when the field IS one of the platform's columns.
 *
 * **The authority is the charter, not the descriptor, and that is the point.**
 * `crates/zeroship-migrate-server/src/apply.rs` says outright that the
 * descriptor is client-declared - a creator who hand-edits the generated files
 * can make them agree about a lie - so a binding read out of the descriptor
 * would be a binding the creator controls. Reading it from the operator charter
 * instead means a hand-edited `.zship` cannot re-point who computes `id`.
 *
 * Matching is by COLUMN NAME because that is what a v2 descriptor is keyed by:
 * its fields are already-resolved wire `FieldDef`s under snake_case column
 * names (see `RuntimeCollectionDescriptorV2`). A creator's own field never
 * reaches this branch under a platform name - `normalizeSchema` refuses those
 * below - so a match here is a platform column, not a collision.
 *
 * An `assign` already present on the def is left alone rather than overwritten,
 * so that when the descriptor starts carrying bindings itself (the mirror the
 * worker verifies) this function does not silently mask a disagreement between
 * the two. Today no descriptor carries one.
 */
function withPlatformAssignment(name: string, def: FieldDef): FieldDef {
  const assign = CONFINED_SYSTEM_SHAPE_ASSIGNMENTS[name];
  if (assign === undefined || def.assign !== undefined) return { ...def };
  return { ...def, assign };
}

/**
 * Converts a SchemaInput into a NormalizedSchema. Every field value
 * must be a `TypeBuilder` produced by the `t.*` API (or the whole
 * input may be a single top-level `t.union(...)` — proposal §C2).
 * Any other shape throws.
 *
 * **P7 PR 1** — refuses any field whose name collides with a
 * platform system field. The Rust-side `field_to_column` would also
 * refuse such schemas at descriptor-install time; throwing here lets
 * `pnpm dev` surface the error immediately on first build.
 */
export function normalizeSchema(input: SchemaInputOrUnion): NormalizedSchema {
  // C2 — top-level discriminated union.
  if (isTypeBuilder(input)) {
    const def = input.toFieldDef();
    if (def.type === "union") {
      return expandUnionToFlatColumns(def);
    }
    throw Object.assign(
      new Error(
        `normalizeSchema: top-level TypeBuilder must be a t.union(...) (got type "${def.type}")`,
      ),
      { code: "SCHEMA_TOP_LEVEL_NOT_UNION" as const },
    );
  }
  const result: NormalizedSchema = {};

  const fields = input as SchemaFieldRecord;
  for (const [key, rawVal] of Object.entries(fields)) {
    // **Migration-first cutover (P4b)** — the bundled RuntimeSchemaDescriptor
    // supplies platform-generated wire `FieldDef`s (not t.* builders). They
    // legitimately carry system fields (id/created_at/version/…) the migration
    // fold materialised, so they bypass the creator-facing system-field fence
    // below (which guards only user-authored t.* schemas) AND the
    // "must be a t.* builder" check. Pass them through verbatim. A TypeBuilder
    // is never an `isFieldDef` candidate here (the `!isTypeBuilder` guard keeps
    // user schemas on the strict path even if a builder exposed a string
    // `type`).
    if (!isTypeBuilder(rawVal) && isFieldDef(rawVal)) {
      result[key] = withPlatformAssignment(key, rawVal as FieldDef);
      continue;
    }
    // **P7 PR 1** — refuse creator-declared fields whose names collide
    // with the seven platform system fields. The Rust-side validator
    // (`validate_field_name_for_declaration`) enforces the same fence
    // while installing the descriptor; the SDK-side check surfaces the error at
    // `pnpm dev` build time so creators don't wait for a worker
    // round-trip. Error code mirrors the Rust-side
    // `RESERVED_SYSTEM_FIELD_NAME`.
    if (SYSTEM_FIELD_NAMES.includes(key)) {
      // Sanctioned exception: `id: t.id("prefix")` is a PREFIX
      // DECLARATION for the always-present system `id` PK column — not
      // an attempt to override the column. Allow it through ONLY when
      // the value is a `type:"id"` builder; the `{type:"id", idPrefix}`
      // def then reaches the runtime descriptor so the auto-mint pass can
      // read the declared prefix. Any other type
      // declared under `id`, and all six other system names, stay
      // rejected. The Rust column emitter skips this field (no duplicate
      // `id` column) and its validator mirrors the `usr` fence.
      const isIdPrefixDecl =
        key === "id" &&
        isTypeBuilder(rawVal) &&
        rawVal.toFieldDef().type === "id";
      if (!isIdPrefixDecl) {
        throw Object.assign(
          new Error(
            `Field name "${key}" is reserved for platform system fields. ` +
              `System fields (${SYSTEM_FIELD_NAMES.join(", ")}) are managed by ` +
              `the platform and cannot be overridden.`,
          ),
          { code: "RESERVED_SYSTEM_FIELD_NAME" as const },
        );
      }
    }
    if (!isTypeBuilder(rawVal)) {
      throw Object.assign(
        new Error(
          `unrecognized schema field "${key}": every field must be a t.* builder ` +
            `(e.g. t.string(), t.number(), t.ref("users")). Bare constructors and ` +
            `Mongoose-style { type: Constructor } objects are no longer supported.`,
        ),
        { code: "SCHEMA_FIELD_NOT_TYPEBUILDER" as const },
      );
    }
    result[key] = { ...rawVal.toFieldDef() };
  }

  return result;
}

/**
 * Expand a top-level `t.union(...)` into a flat `NormalizedSchema`
 * (proposal §C2). See `docs/archive/zeroship-db.md` for the full
 * rules; in short: every non-discriminator field becomes a nullable
 * top-level column, fields shared across variants must agree on
 * `type`, and the discriminator becomes a NOT NULL column with an
 * `enum` constraint listing every variant's literal value.
 */
export function expandUnionToFlatColumns(def: FieldDef): NormalizedSchema {
  if (def.type !== "union" || def.variants === undefined || def.discriminator === undefined) {
    throw Object.assign(
      new Error("expandUnionToFlatColumns: not a union FieldDef"),
      { code: "UNION_EXPAND_NOT_UNION" as const },
    );
  }
  const discriminator = def.discriminator;
  const variants = def.variants as Record<string, FieldDef>[];
  const result: NormalizedSchema = {};

  const discValues: (string | number | boolean)[] = [];
  let discPrimType: "string" | "number" | "boolean" | null = null;
  for (let i = 0; i < variants.length; i++) {
    const fd = variants[i][discriminator];
    if (fd === undefined || fd.type !== "literal" || fd.literalValue === undefined) {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: variant #${i} missing discriminator field "${discriminator}"`,
        ),
        { code: "UNION_VARIANT_MISSING_DISCRIMINATOR" as const },
      );
    }
    const lit = fd.literalValue;
    const primTy = typeof lit;
    if (primTy !== "string" && primTy !== "number" && primTy !== "boolean") {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: discriminator literal of variant #${i} has unsupported type "${primTy}"`,
        ),
        { code: "UNION_DISCRIMINATOR_UNSUPPORTED_TYPE" as const },
      );
    }
    if (discPrimType === null) {
      discPrimType = primTy as "string" | "number" | "boolean";
    } else if (discPrimType !== primTy) {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: discriminator literals across variants must share a primitive type (got "${discPrimType}" and "${primTy}")`,
        ),
        { code: "UNION_DISCRIMINATOR_TYPE_MISMATCH" as const },
      );
    }
    discValues.push(lit);
  }
  const seen = new Set<string>();
  for (const v of discValues) {
    const tag = typeof v + ":" + String(v);
    if (seen.has(tag)) {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: duplicate discriminator value ${JSON.stringify(v)}`,
        ),
        { code: "UNION_DUPLICATE_DISCRIMINATOR_VALUE" as const },
      );
    }
    seen.add(tag);
  }

  result[discriminator] = {
    type: discPrimType ?? "string",
    required: true,
    enum: discValues as (string | number)[],
    discriminator: "__discriminator__",
    variants: variants.map((variant) => {
      const cloned: Record<string, FieldDef> = {};
      for (const [k, fieldDef] of Object.entries(variant)) {
        cloned[k] = { ...fieldDef };
      }
      return cloned;
    }),
  };

  for (const variant of variants) {
    for (const [field, fieldDef] of Object.entries(variant)) {
      if (field === discriminator) continue;
      const existing = result[field];
      if (existing === undefined) {
        const expanded: FieldDef = { ...fieldDef, required: false };
        result[field] = expanded;
      } else {
        if (existing.type !== fieldDef.type) {
          throw Object.assign(
            new Error(
              `expandUnionToFlatColumns: field "${field}" has incompatible types across variants ("${existing.type}" vs "${fieldDef.type}")`,
            ),
            { code: "UNION_FIELD_TYPE_MISMATCH" as const },
          );
        }
      }
    }
  }

  return result;
}

/**
 * Verify that every `t.ref("table")` in `schemas` points at a
 * collection that is itself declared in `schemas`. Throws an `Error`
 * with `code = "REF_TARGET_NOT_FOUND"` on the first violation.
 *
 * String-based (not type-based) so it acts as a safety net for
 * `t.ref("x" as any)` escapes that bypass the compile-time
 * `Tables<S>` constraint.
 *
 * This is a MEMBERSHIP test, not a cross-app rule. A qualified target
 * such as `"other_app.users"` does fail here, but only because it is
 * not a key of this app's schema map, and the thrown message says
 * "not declared in the schema map ... or fix the typo" accordingly.
 * Nothing here inspects the target for an app prefix, and this check
 * does not run at all for schema applied by the migration engine at
 * deploy. What structurally keeps an FK inside one app is the DDL
 * renderer in `crates/zeroship-migrate-core/src/schema/query.rs` -- see the foreign
 * keys section of `docs/reference/db.md`.
 */
export function validateRefTargets(
  schemas: Record<string, unknown>,
): void {
  const declaredCollections = new Set(Object.keys(schemas));
  const reportMissing = (
    collection: string,
    field: string,
    target: string,
  ): never => {
    const message =
      `t.ref("${target}") on ${collection}.${field} — ` +
      `target collection "${target}" is not declared in the schema map. ` +
      `Add "${target}" to the schema map, or fix the typo.`;
    throw Object.assign(new Error(message), {
      code: "REF_TARGET_NOT_FOUND",
      collection,
      field,
      target,
    });
  };

  const walkFieldDef = (
    collectionName: string,
    path: string,
    fd: FieldDef,
  ): void => {
    if (fd.type === "ref" && fd.refTarget !== undefined && !declaredCollections.has(fd.refTarget)) {
      reportMissing(collectionName, path, fd.refTarget);
    }
    if (fd.type === "object" && fd.shape !== undefined) {
      for (const [sub, subDef] of Object.entries(fd.shape)) {
        walkFieldDef(collectionName, `${path}.${sub}`, subDef);
      }
    }
    if (fd.type === "union" && fd.variants !== undefined) {
      for (const variant of fd.variants) {
        for (const [sub, subDef] of Object.entries(variant)) {
          walkFieldDef(collectionName, `${path}.${sub}`, subDef);
        }
      }
    }
  };

  for (const [collectionName, rawSchema] of Object.entries(schemas)) {
    if (isTypeBuilder(rawSchema)) {
      const fd = rawSchema.toFieldDef();
      if (fd.type === "union" && fd.variants !== undefined) {
        for (const variant of fd.variants) {
          for (const [field, vDef] of Object.entries(variant)) {
            walkFieldDef(collectionName, field, vDef);
          }
        }
      }
      continue;
    }
    const fields =
      isSchemaBuilder(rawSchema)
        ? (rawSchema as SchemaBuilder<Record<string, unknown>>).fields
        : rawSchema;
    if (fields === null || typeof fields !== "object") continue;
    for (const [field, def] of Object.entries(fields as PlainObject)) {
      if (isTypeBuilder(def)) {
        walkFieldDef(collectionName, field, def.toFieldDef());
      } else if (isFieldDef(def)) {
        walkFieldDef(collectionName, field, def);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// model() factory (moved from @zeroship/db/model)
// ---------------------------------------------------------------------------

/**
 * Creates a Collection for the given collection name and schema.
 *
 * Framework-internal — `installSchema` is the only caller in
 * production. Tests construct collections through `installSchema`'s
 * return value too. If a user needs a one-off Collection outside of
 * `installSchema`, they should declare schema on the entry and read
 * `env.db.<name>` — that's the supported path.
 */
export function model<S extends Record<string, unknown>>(
  name: string,
  schema: S,
  native: NativeDb,
  namingStrategy: NamingStrategy = naming.asIs,
  softDelete: boolean = false,
  versioning: boolean = false,
  declaredIndexes: readonly NamedIndexSpec[] = [],
): Collection<S> {
  if (typeof name !== "string" || name.trim().length === 0) {
    throw Object.assign(
      new Error("model name must be a non-empty string"),
      { code: "MODEL_INVALID_NAME" as const },
    );
  }
  if (schema === null || schema === undefined || typeof schema !== "object") {
    throw Object.assign(
      new Error("model schema must be an object"),
      { code: "MODEL_INVALID_SCHEMA" as const },
    );
  }
  // C2 — a top-level `t.union(...)` is a valid schema. normalizeSchema
  // detects the TypeBuilder branch and expands the union; works for
  // both record-of-fields and a top-level `t.union(...)` input.
  const normalized = normalizeSchema(schema as Parameters<typeof normalizeSchema>[0]);

  // Both injections below run AFTER `normalizeSchema`, which is where
  // `withPlatformAssignment` stamps the charter's `assign` onto a platform
  // column. So each must take the stamp explicitly, or it arrives with no
  // `assign` and `validateDoc` falls through to the `default` arm - which
  // MATERIALISES the value into the caller's document.
  //
  // That is harmless for `deletedAt`, which declares no default, and is not for
  // `version`: its `default: 1` is the DDL seed, and the design forbids that key
  // reaching `build_upsert`, where the generic loop emits
  // `"version" = EXCLUDED."version"` while the auto-bump emits a second
  // assignment to the same column - two assignments to one column in one
  // `DO UPDATE SET`, which PostgreSQL refuses. Measured before this fix: an
  // insert reached the native op as `{"title":"hello","version":1}`.

  // When soft delete is enabled, inject the deletedAt field into the schema
  if (softDelete && !normalized.deletedAt) {
    normalized.deletedAt = withPlatformAssignment("deletedAt", {
      type: "date",
      required: false,
    });
  }

  // D4 — when versioning is enabled, inject a `version` column.
  if (versioning && !normalized.version) {
    normalized.version = withPlatformAssignment("version", {
      type: "number",
      required: false,
      default: 1,
    });
  }

  return new Collection<S>(name, normalized, native, {
    naming: namingStrategy,
    softDelete,
    versioning,
    indexes: declaredIndexes,
  });
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/**
 * Schema definition — plain fields, schema() builder with options, or
 * a top-level `t.union(...)` whose row shape is a discriminated union
 * (proposal §C2). The TypeBuilder form is type-erased to
 * `TypeBuilder<unknown, any>` here so the conditional in `UnwrapSchema`
 * can distribute over the union.
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type SchemaInput =
  | Record<string, unknown>
  | SchemaBuilder<Record<string, unknown>>
  | TypeBuilder<unknown, any, any, any, any>;

/**
 * Schema-shape validator. When a user writes `{ name: "string" }`
 * instead of `{ name: t.string() }`, the bare value would otherwise
 * pass the `Record<string, unknown>` constraint silently. This
 * validator walks each collection's field map and emits a
 * string-literal error type at the offending field.
 */
type IsValidSchemaField<F> = F extends TypeBuilder<unknown, boolean, any, any, any> ? true : false;

export type ValidateSchemaShape<T> = {
  [K in keyof T]:
    T[K] extends SchemaBuilder<Record<string, unknown>>
      ? T[K]
      : T[K] extends TypeBuilder<unknown, boolean, any, any, any>
        ? T[K]
        : T[K] extends Record<string, unknown>
          ? {
              [F in keyof T[K]]:
                IsValidSchemaField<T[K][F]> extends true
                  ? T[K][F]
                  : `Field "${F & string}" on "${K & string}" must be a t.* builder (e.g. t.string(), t.number(), t.ref("users")) — got a bare value.`;
            }
          : `Schema "${K & string}" must be a field map of t.* builders, a schema(...) builder, or a top-level t.union(...).`;
};

type TxPaginationResult<P> = {
  page: P[];
  continueCursor: string;
  isDone: boolean;
};

/**
 * A typed collection inside a transaction — same API as Collection but
 * throws on error instead of returning Result. Generic over schema
 * shape S.
 */
export type TxCollection<S = PlainObject, AllSchemas extends Record<string, unknown> = Record<string, unknown>> = {
  insert(row: RowInput<S>): Promise<Row<S>>;
  insertMany(rows: RowInput<S>[]): Promise<Row<S>[]>;
  get<K extends string & keyof Row<S>>(
    idOrFilter: string | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Pick<Row<S>, K> | null>;
  get<W extends WithSpec>(
    idOrFilter: string | Filter<S>,
    opts: { with: W; orderBy?: Record<string, 1 | -1> },
  ): Promise<(Row<S> & WithRelations<S, W, AllSchemas>) | null>;
  get(
    idOrFilter: string | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find<W extends WithSpec>(filter: Filter<S>, opts: { with: W }): TxQuery<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): TxQuery<S, Row<S>, AllSchemas>;
  upsert(row: RowInput<S>, options: UpsertOptions<S>): Promise<Row<S>>;
  update(idOrFilter: string | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
  delete(idOrFilter: string | Filter<S>): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  purge(idOrFilter: string | Filter<S>): Promise<Row<S> | null>;
  purgeMany(filter?: Filter<S>): Promise<{ purgedCount: number }>;
  restore(idOrFilter: string | Filter<S>): Promise<Row<S> | null>;
  restoreMany(filter?: Filter<S>): Promise<{ restoredCount: number }>;
  count(filter?: Filter<S>): Promise<number>;
  distinct(field: string & keyof Row<S>, filter?: Filter<S>): Promise<(string | number | boolean | null)[]>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<PlainObject[]>;
  bulkUnmask(
    items: ReadonlyArray<{
      id: string;
      columns: readonly (string & keyof Row<S>)[];
    }>,
    opts: { actor: Record<string, unknown>; reason?: string },
  ): Promise<Map<string, Record<string, unknown>>>;
  search(
    args: {
      vector: number[];
      k?: number;
      metric?: "cosine" | "l2" | "innerProduct";
      column?: string;
      filter?: Filter<S>;
    },
  ): Promise<(Row<S> & { _distance?: number })[]>;
  near(args: {
    field: keyof S & string;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  }): Promise<(Row<S> & { _distance_m: number })[]>;
};

/** Query inside a transaction — same chainable API but resolves to data directly */
export type TxQuery<
  S = PlainObject,
  P = Row<S>,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> = {
  sort(s: Record<string, number> | string): TxQuery<S, P, AllSchemas>;
  limit(n: number): TxQuery<S, P, AllSchemas>;
  skip(n: number): TxQuery<S, P, AllSchemas>;
  select<K extends keyof Row<S> & string>(fields: K[]): TxQuery<S, Pick<Row<S>, K>, AllSchemas>;
  select(s: string | string[] | Record<string, number | boolean>): TxQuery<S, P, AllSchemas>;
  after(id: string): TxQuery<S, P, AllSchemas>;
  with<W extends WithSpec>(spec: W): TxQuery<S, P & WithRelations<S, W, AllSchemas>, AllSchemas>;
  paginate(opts: {
    cursor?: string | null;
    numItems: number;
  }): Promise<TxPaginationResult<P>>;
  /** **P9 PR 1** — first matching row or `null`; throws on native error. */
  first(): Promise<P | null>;
  /** **P9 PR 1** — strict exactly-one; throws `NotFoundError` on zero or
   *  `NotUniqueError` on >1 matches. */
  unique(): Promise<P>;
  /** **P9 PR 1** — last matching row in the current sort, or `null`;
   *  throws `InvalidOperationError` if no sort was set. */
  last(): Promise<P | null>;
  then<TResult1 = P[], TResult2 = never>(
    resolve?: ((value: P[]) => TResult1 | PromiseLike<TResult1>) | null,
    reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null
  ): Promise<TResult1 | TResult2>;
};

/** Options for the transaction method. */
export interface TransactionOptions {
  isolationLevel?: IsolationLevel;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type UnwrapSchema<T> =
  T extends SchemaBuilder<infer S> ? S :
  T extends TypeBuilder<infer U, any, any, any, any> ? U :
  T;

export type Collections<T extends Record<string, SchemaInput>> = {
  [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T>;
};

export type DbExtensions<T extends Record<string, SchemaInput>> = {
  transaction: <R>(fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>, T> }) => Promise<R>, options?: TransactionOptions) => Promise<Result<R>>;
  live: <R>(queryFn: () => Promise<R[]> | { then(onFulfilled: (value: unknown) => unknown, onRejected?: (reason: unknown) => unknown): unknown }, options?: LiveOptions) => LiveQuery<R>;
};

export type Db<T extends Record<string, SchemaInput>> = Collections<T> & DbExtensions<T>;

// ---------------------------------------------------------------------------
// TxCollection — wraps a Collection, throws on error
// ---------------------------------------------------------------------------

async function unwrap<T>(result: Result<T>): Promise<T> {
  if (result.error) throw result.error;
  return result.data as T;
}

function createTxCollection<S>(collection: Collection<S>): TxCollection<S> {
  async function getImpl(
    idOrFilter: string | Filter<S>,
    opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<unknown> {
    const colAny = collection as unknown as {
      get(
        idOrFilter: string | Filter<S>,
        opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
      ): Promise<Result<Row<S> | null>>;
    };
    return unwrap(await colAny.get(idOrFilter, opts));
  }

  const tx: TxCollection<S> = {
    async insert(row: RowInput<S>) {
      return unwrap(await collection.insert(row));
    },
    async insertMany(rows: RowInput<S>[]) {
      return unwrap(await collection.insertMany(rows));
    },
    get: getImpl as TxCollection<S>["get"],
    async exists(filter: Filter<S>) {
      return unwrap(await collection.exists(filter));
    },
    find: ((filter: Filter<S> = {} as Filter<S>, opts?: { with?: WithSpec }): TxQuery<S, Row<S>> => {
      const query = (opts?.with !== undefined
        ? (collection as unknown as {
            find(f: Filter<S>, o: { with: WithSpec }): Query<S, Row<S>>;
          }).find(filter, { with: opts.with })
        : collection.find(filter));
      return createTxQuery<S>(query);
    }) as TxCollection<S>["find"],
    async upsert(row: RowInput<S>, options: UpsertOptions<S>) {
      return unwrap(await collection.upsert(row, options));
    },
    async update(idOrFilter: string | Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.update(idOrFilter, patch));
    },
    async updateMany(filter: Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.updateMany(filter, patch));
    },
    async delete(idOrFilter: string | Filter<S>) {
      return unwrap(await collection.delete(idOrFilter));
    },
    async deleteMany(filter: Filter<S>) {
      return unwrap(await collection.deleteMany(filter));
    },
    async purge(idOrFilter: string | Filter<S>) {
      return unwrap(await collection.purge(idOrFilter));
    },
    async purgeMany(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.purgeMany(filter));
    },
    async restore(idOrFilter: string | Filter<S>) {
      return unwrap(await collection.restore(idOrFilter));
    },
    async restoreMany(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.restoreMany(filter));
    },
    async count(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.count(filter));
    },
    async distinct(field: string & keyof Row<S>, filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.distinct(field, filter));
    },
    async aggregate(pipeline: ZeroshipDbAggregateStage[]) {
      return unwrap(await collection.aggregate(pipeline));
    },
    async bulkUnmask(
      ...args: Parameters<Collection<S>["bulkUnmask"]>
    ) {
      return unwrap(await collection.bulkUnmask(...args));
    },
    async search(
      ...args: Parameters<Collection<S>["search"]>
    ) {
      return unwrap(await collection.search(...args));
    },
    async near(
      ...args: Parameters<Collection<S>["near"]>
    ) {
      return unwrap(await collection.near(...args));
    },
  };
  return tx;
}

function createTxQuery<S>(query: Query<S, Row<S>>): TxQuery<S, Row<S>> {
  function selectImpl(
    s: string | string[] | Record<string, number | boolean>,
  ): unknown {
    (query.select as (arg: unknown) => unknown)(s);
    return wrapped;
  }
  const wrapped: TxQuery<S, Row<S>> = {
    sort(s: Record<string, number> | string) { query.sort(s); return wrapped; },
    limit(n: number) { query.limit(n); return wrapped; },
    skip(n: number) { query.skip(n); return wrapped; },
    select: selectImpl as TxQuery<S, Row<S>>["select"],
    after(id: string) { query.after(id); return wrapped; },
    with: ((spec: WithSpec) => {
      (query as unknown as { with(s: WithSpec): unknown }).with(spec);
      return wrapped;
    }) as TxQuery<S, Row<S>>["with"],
    async paginate(
      opts: Parameters<Query<S, Row<S>>["paginate"]>[0],
    ): Promise<TxPaginationResult<Row<S>>> {
      return unwrap(await query.paginate(opts));
    },
    // **P9 PR 1** — Result→throw shims for the new terminals so the
    // tx-callback contract (throw, not return Result) stays uniform.
    async first(): Promise<Row<S> | null> {
      return unwrap(await query.first());
    },
    async unique(): Promise<Row<S>> {
      return unwrap(await query.unique());
    },
    async last(): Promise<Row<S> | null> {
      return unwrap(await query.last());
    },
    then<TResult1 = Row<S>[], TResult2 = never>(
      resolve?: ((value: Row<S>[]) => TResult1 | PromiseLike<TResult1>) | null,
      reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
    ): Promise<TResult1 | TResult2> {
      return query.then(
        (result: Result<Row<S>[]>) => {
          if (result.error) throw result.error;
          return resolve ? resolve(result.data as Row<S>[]) : (result.data as unknown as TResult1);
        },
        reject,
      ) as Promise<TResult1 | TResult2>;
    },
  };
  return wrapped;
}

// ---------------------------------------------------------------------------
// installSchema
// ---------------------------------------------------------------------------

export interface InstallSchemaOptions {
  /**
   * Column naming strategy. Default: `naming.asIs` — the descriptor field name
   * IS the column name.
   *
   * This used to default to `naming.snakeCase`, and that was the only renaming
   * step in the entire migration-first pipeline. Nothing upstream produced the
   * name it expected:
   *
   *   - the engine renders the authored migration field name VERBATIM as the
   *     column (`userId: t.text()` creates a column spelled `"userId"`);
   *   - gen-types folds that name into the descriptor verbatim;
   *   - `render-env-db.ts` performs no case conversion, so the generated
   *     `env.db` TypeScript field is that same name;
   *   - and then this mapped it to `user_id` on the wire.
   *
   * A creator authoring any camelCase field therefore got an app that builds,
   * boots, and fails every data call with `table <t> has no column named
   * <snake_cased>`. It went unnoticed because all 40+ platform migrations
   * author snake_case, on which the mapping is the identity: measured across
   * the 17 committed `schema.runtime.json` descriptors, 169 fields, exactly
   * ONE (`db-todos`'s `todos.userId`) is changed by `snakeCase` at all. So
   * this change is inert for every other schema in the tree by measurement,
   * not by argument.
   *
   * `collection.ts` already defaults the same construction to `naming.asIs`;
   * this makes the two agree. Pass `naming: naming.snakeCase` explicitly to opt
   * into camelCase-field/snake_case-column mapping.
   */
  naming?: NamingStrategy;
  /**
   * **Migration-first cutover (P5 S3)** — the bundled
   * {@link RuntimeSchemaDescriptor}, resolved from `manifest.runtime_descriptor`
   * and injected by the runtime as `globalThis.__zsRuntimeDescriptor`. v2 carries
   * `{ fields, options, indexes }` per collection and is the schema SOURCE OF
   * TRUTH. The first `schemas` argument is no longer consulted for fields or
   * collection options; absent descriptor means schema-less install.
   */
  descriptor?: RuntimeSchemaDescriptor;
}

const RESERVED_ENV_DB_NAMES = new Set<string>([
  "collection",
  "migrations",
  "replication",
  // **P9 PR 3** — `beginTransaction` removed: the native primitive was
  // deleted entirely (transaction orchestration moved into Rust). The
  // creator-facing `transaction` (below) is now a native method on
  // `env.db`, so it stays reserved.
  "openSubscription",
  "transaction",
  "live",
]);

let _installInFlight = false;

/**
 * Framework-internal helper that installs the runtime schema descriptor.
 * Returns the typed collection map after planting it on `env.db`.
 */
export function installSchema<const T extends Record<string, SchemaInput>>(
  schemas: ValidateSchemaShape<T>,
  env: NativeDb,
  options?: InstallSchemaOptions,
): { collections: Collections<T> } {
  if (_installInFlight) {
    throw Object.assign(
      new Error(
        "@zeroship/bootstrap: installSchema called while a previous install is in flight — " +
          "this helper must run from a single-threaded scope (the dev-bootstrap and " +
          "production synthetic SSR entry both serialize).",
      ),
      { code: "INSTALL_IN_FLIGHT" as const },
    );
  }
  _installInFlight = true;
  try {
    return _installSchemaInner(schemas as unknown as T, env, options);
  } finally {
    _installInFlight = false;
  }
}

function _installSchemaInner<const T extends Record<string, SchemaInput>>(
  schemas: T,
  env: NativeDb,
  options?: InstallSchemaOptions,
): { collections: Collections<T> } {
  if (env == null || typeof env !== "object") {
    throw Object.assign(
      new Error(
        "@zeroship/bootstrap: installSchema requires a native env.db handle as " +
          "the second argument — got " + (env === undefined ? "undefined" : env === null ? "null" : typeof env) + ".",
      ),
      { code: "NATIVE_DB_UNAVAILABLE" as const },
    );
  }
  const native = env;
  const namingStrategy = options?.naming ?? naming.asIs;

  // **Migration-first cutover (P5 S6)** — descriptor v2 is the only runtime
  // schema source. The declared first argument is ignored. An absent descriptor
  // installs no collections; a present but non-v2 descriptor is a hard error.
  const descriptor = options?.descriptor;
  const descriptorV2 = assertRuntimeDescriptorV2(descriptor);
  const descriptorFields = runtimeDescriptorFields(descriptor);
  const source: T =
    descriptorFields !== null
      ? (descriptorFields as unknown as T)
      : ({} as T);

  const collections = {} as { [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T> };

  // **P9 PR 3** — capture the *native* `Db.transaction(callback, opts)`
  // method BEFORE the install loop overwrites `env.db.transaction` with
  // the bootstrap `transactionImpl` wrapper.
  //
  // The hazard: the install loop does `Object.defineProperty(native,
  // "transaction", transactionImpl)`, planting an OWN property that
  // shadows the native prototype method. A naive `native.transaction`
  // read on a *re-install* would then resolve to the previously-installed
  // `transactionImpl` (own property) — and `transactionImpl` calling
  // itself recurses forever.
  //
  // Fix: stash the captured native method under a non-enumerable hidden
  // key the first time, and reuse it on every subsequent install. The
  // first capture reads `native.transaction` before any own property is
  // planted, so it picks up the real native orchestrator (in production a
  // `Db.prototype` method; in tests a mock's own `transaction`). Bound to
  // `native` so the v8_class receiver check passes.
  const NATIVE_TX_KEY = "__zsNativeTransaction";
  const nativeTransaction = captureNativeTransaction(
    native as unknown as object,
    NATIVE_TX_KEY,
  ) as NativeTransactionFn | undefined;

  validateRefTargets(source);

  // P5 S3: v2 descriptors carry collection-level options directly.
  const collectionOptionsFor = (
    name: string,
  ): {
    softDelete: boolean;
    versioning: boolean;
    indexes: readonly NamedIndexSpec[];
  } => {
    const fromDescriptor = descriptorV2?.collections[name];
    if (fromDescriptor !== undefined) {
      return {
        softDelete: fromDescriptor.options?.softDelete ?? false,
        versioning: fromDescriptor.options?.versioning ?? false,
        indexes: fromDescriptor.indexes ?? [],
      };
    }
    return { softDelete: false, versioning: false, indexes: [] };
  };

  for (const [name, rawSchema] of Object.entries(source)) {
    const isBuilder = rawSchema instanceof SchemaBuilder;
    const fields = isBuilder ? rawSchema.fields : rawSchema;
    const collectionFields = fields;
    const opts = collectionOptionsFor(name);
    (collections as Record<string, Collection<unknown, string, T>>)[name] =
      model(
        name,
        collectionFields as Record<string, unknown>,
        native,
        namingStrategy,
        opts.softDelete,
        opts.versioning,
        opts.indexes,
      ) as Collection<unknown, string, T>;
  }

  const resolveCollection = (n: string): Collection<unknown> | undefined =>
    (collections as Record<string, Collection<unknown>>)[n];
  for (const col of Object.values(collections)) {
    (col as unknown as {
      _setResolveCollection(fn: (name: string) => Collection<unknown> | undefined): void;
    })._setResolveCollection(resolveCollection);
  }

  const txCollections = {} as { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>, T> };
  for (const [name, col] of Object.entries(collections)) {
    (txCollections as Record<string, TxCollection<unknown>>)[name] =
      createTxCollection(col as Collection<unknown>);
  }

  function liveImpl<R>(
    queryFn: () => Promise<R[]> | { then(onFulfilled: (value: unknown) => unknown, onRejected?: (reason: unknown) => unknown): unknown },
    liveOptions?: LiveOptions,
  ): LiveQuery<R> {
    return createLive<R>(collections as Record<string, unknown>, queryFn, liveOptions);
  }

  // **P9 PR 3** — transaction orchestration moved into Rust.
  //
  // The native `env.db.transaction(callback, opts)` v8_method owns
  // begin / commit / rollback / nested-savepoint (see
  // `crates/zeroship-data-orm/src/transaction/mod.rs`). It calls
  // `callback(rawTxView)` once BEGIN/SAVEPOINT succeeds and returns a
  // promise that resolves with the callback's result on commit (callback
  // resolved) or rejects with the callback's error on rollback (callback
  // threw). BEGIN, classified session-setup, commit, savepoint, and body
  // errors are emitted by Rust and surface verbatim on the rejection
  // (`err.code`, plus `err.status` when the classification has an HTTP
  // remedy).
  //
  // This wrapper keeps only the JS-side concerns that have no Rust
  // counterpart:
  //   1. **DataLoader drain** — the per-collection `IdLoader` microtask
  //      queues are pure JS state; we flush them before opening the tx so
  //      a batched `get(id)` issued just before `transaction(...)`
  //      completes on the pool, not the tx connection.
  //   2. **`_txDepth` bookkeeping** — bumped on every collection while the
  //      tx is open so (a) the IdLoader's `LOADER_TX_RACE` guard rejects a
  //      non-tx batched read that finds a tx opened mid-batch, and (b)
  //      `live()` refuses with `LIVE_IN_TRANSACTION` when called inside a
  //      tx body. (The Rust orchestrator owns *nesting depth*; this JS
  //      counter is purely the "am I inside a tx on this collection"
  //      signal those two JS-layer checks read.)
  //   3. **`Result` wrapping** — `transaction(fn)` returns
  //      `Promise<Result<R>>`; the native promise resolves/rejects, so we
  //      adapt resolve → `ok`, reject → `err`.
  //
  // The `txCollections` (SDK collections wrapped `Result`→throw) route
  // through the tx connection automatically, since the native CRUD path
  // consults the `tx_conn` slot the orchestrator set. We therefore pass
  // `txCollections` to the creator callback and ignore the native
  // `rawTxView` (its collections are the same connection; the SDK
  // wrappers add the field-mapping + throwing contract the callback
  // expects).
  async function transactionImpl<R>(
    fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>, T> }) => Promise<R>,
    txOptions?: TransactionOptions,
  ): Promise<Result<R>> {
    if (nativeTransaction === undefined) {
      return err(
        Object.assign(
          new Error(
            "@zeroship/bootstrap: env.db.transaction not available — " +
              "runtime is missing the native Db.transaction(fn) orchestrator.",
          ),
          { code: "NATIVE_TRANSACTION_UNAVAILABLE" as const },
        ),
      );
    }

    const collectionList = Object.values(collections);

    // 1. Drain the JS DataLoader queues (JS-only state; cannot move to
    //    Rust). A drain failure aborts before any BEGIN runs.
    try {
      await drainCollectionLoaders(collectionList);
    } catch (drainErr) {
      const wrapped = Object.assign(
        new Error(
          `pre-transaction drain failed: ${
            drainErr instanceof Error ? drainErr.message : String(drainErr)
          }`,
          { cause: drainErr instanceof Error ? drainErr : undefined },
        ),
        { code: "TX_DRAIN_FAILED" as const },
      );
      return err(wrapped);
    }

    // 2. Mark every collection in-tx so the loader race-guard +
    //    live-in-tx refusal see depth > 0 for the duration. Bumped
    //    synchronously *before* the native call so an in-flight batched
    //    read observes the tx the instant BEGIN opens.
    const txScopedCollections = enterTransactionScope(collectionList);
    try {
      // 3. Native orchestrator: begin → callback(txCollections) →
      //    commit/rollback. Resolves with the callback's result on
      //    commit; rejects with the typed error on rollback, setup denial,
      //    begin failure, commit indeterminacy, or depth exhaustion.
      const opts = txOptions?.isolationLevel
        ? { isolationLevel: txOptions.isolationLevel }
        : undefined;
      const bodyResult = (await nativeTransaction(
        // The native view's collections share the tx connection, so we
        // hand the creator our SDK-wrapped `txCollections` (Result→throw
        // + field mapping). `rawTxView` is intentionally unused.
        (_rawTxView: unknown) => fn(txCollections),
        opts,
      )) as R;
      return ok(bodyResult);
    } catch (txErr) {
      // The native rejection already carries the right code
      // (`GRANT_REVOKED`, commit/begin/savepoint codes, or a future setup
      // fence) or is the creator's own thrown error verbatim. Surface it as
      // `result.error`.
      return err(txErr instanceof Error ? txErr : new Error(String(txErr)));
    } finally {
      exitTransactionScope(txScopedCollections);
    }
  }

  {
    const target = native as unknown as Record<string, unknown> & {
      __zeroshipDbInstalledNames?: string[];
    };
    const newNames = Object.keys(collections);
    const newNameSet = new Set(newNames);
    const prevNames = Array.isArray(target.__zeroshipDbInstalledNames)
      ? target.__zeroshipDbInstalledNames
      : [];
    for (const stale of prevNames) {
      if (newNameSet.has(stale)) continue;
      if (RESERVED_ENV_DB_NAMES.has(stale)) continue;
      try {
        delete target[stale];
      } catch {
        /* native v8_class may refuse the delete on a sealed prototype */
      }
    }
    for (const [name, col] of Object.entries(collections)) {
      if (RESERVED_ENV_DB_NAMES.has(name)) {
        throw Object.assign(
          new Error(
            `@zeroship/bootstrap: schema name "${name}" collides with a native env.db method — ` +
              `rename the collection. Reserved: ${[...RESERVED_ENV_DB_NAMES].join(", ")}.`,
          ),
          { code: "RESERVED_ENV_DB_NAME" as const },
        );
      }
      Object.defineProperty(target, name, {
        value: col,
        configurable: true,
        enumerable: true,
        writable: false,
      });
    }
    Object.defineProperty(target, "transaction", {
      value: transactionImpl,
      configurable: true,
      enumerable: true,
      writable: false,
    });
    Object.defineProperty(target, "live", {
      value: liveImpl,
      configurable: true,
      enumerable: true,
      writable: false,
    });
    Object.defineProperty(target, "__zeroshipDbInstalledNames", {
      value: newNames,
      configurable: true,
      enumerable: false,
      writable: true,
    });
  }

  return { collections: collections as Collections<T> };
}
