/**
 * `installSchema` — framework-internal helper behind the
 * `export default { schema }` convention. Stage 7 of the refactor moved
 * this out of `@zeroship/db` into `@zeroship/bootstrap` so the same
 * implementation backs both the runtime crate's bootstrap and the Vite
 * plugin's dev path. User code MUST NOT call this — declare a schema on
 * the entry's `default` and the platform installs it.
 *
 * Behaviour (mirrors the previous `@zeroship/db::installSchema`):
 *   - Walks the schema map and runs `registerModel` in topological order
 *     so parent tables precede child tables.
 *   - Plants typed `Collection` wrappers PLUS the `transaction` / `live`
 *     extension methods as own properties on the supplied `env` (the
 *     native `ZeroshipDb` handle — `env.db` in production, a mock in
 *     tests). After this returns, `env.<collection>.find(...)` and
 *     `env.transaction(tx => ...)` are live.
 *   - Returns `{ collections, ready }`. The bootstrap (production
 *     `runtime-entry.ts`, dev `dev-entry.ts`) awaits `ready` before
 *     dispatching any request so the auto-tx path doesn't collide with
 *     the orchestrator's `pg_advisory_lock`.
 *
 * Re-entrancy: a second call with overlapping names re-installs the
 * Collection wrappers (`configurable: true` on the descriptors).
 * Reserved native names throw at boot rather than silently shadowing.
 */
import {
  Collection,
  type NativeDb,
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
  type UpdateExpression,
  type Filter,
  type IsolationLevel,
  type WithSpec,
  type WithRelations,
  type PlainObject,
  type FieldDef,
} from "@zeroship/db/internal";

// ---------------------------------------------------------------------------
// DbPlatform capability handle (P9 PR 4 — §8)
// ---------------------------------------------------------------------------

/**
 * The platform-internal capability handle, set on the native `env.db`
 * object under a V8 private symbol and reachable only via the runtime's
 * `globalThis.__zsDbPlatform(db)` resolver (P9 §8). It is NOT a string
 * property on `env.db` — creator code cannot reach it, and it is absent
 * from the published `@zeroship/types` surface (its shape lives in this
 * package's framework-internal `internal.d.ts`).
 *
 * `installSchema` reads it via {@link resolveDbPlatform} and routes
 * `registerModel` / `setMaskPolicy` through it. The matching
 * `ZeroshipDbPlatform` ambient interface (in `internal.d.ts`) carries
 * the full surface (including the `migrations` / `replication`
 * namespaces); this local alias is the subset `installSchema` calls.
 */
export interface DbPlatformHandle {
  registerModel(
    collection: string,
    schema: unknown,
    indexes?: unknown,
  ): Promise<void>;
  setMaskPolicy(policy: Record<string, readonly string[]>): Promise<unknown>;
}

/**
 * Resolve the {@link DbPlatformHandle} for a native `env.db` object via
 * the runtime's `globalThis.__zsDbPlatform(db)` resolver (P9 §8). The
 * resolver reads the handle out of the private-symbol slot on `db`.
 *
 * Returns `undefined` when:
 *   - the resolver isn't installed (no DbPlugin on this runtime — e.g. a
 *     dev run without `DATABASE_URL`, or a unit test with a mock
 *     `env.db`), or
 *   - `db` carries no platform slot (a mock that wasn't minted by the
 *     native `mint_db`).
 *
 * In both cases `installSchema` falls back to its handle-absent path: it
 * skips `registerModel` (the dispatcher's `_schemaReady` defense still
 * gates auto-tx), exactly as it did before P9 PR 4 when `registerModel`
 * lived directly on `env.db`. The `prefer` argument lets a caller (the
 * runtime-entry) pass a handle it resolved earlier so the resolver isn't
 * consulted twice — and so it keeps working after runtime-entry deletes
 * the global.
 */
export function resolveDbPlatform(
  db: unknown,
  prefer?: DbPlatformHandle,
): DbPlatformHandle | undefined {
  if (prefer && typeof prefer.registerModel === "function") return prefer;
  const g = globalThis as unknown as {
    __zsDbPlatform?: (db: unknown) => unknown;
  };
  if (typeof g.__zsDbPlatform !== "function") return undefined;
  const handle = g.__zsDbPlatform(db);
  if (handle == null || typeof handle !== "object") return undefined;
  const h = handle as Partial<DbPlatformHandle>;
  if (typeof h.registerModel !== "function") return undefined;
  return handle as DbPlatformHandle;
}

// ---------------------------------------------------------------------------
// normalizeSchema + expandUnionToFlatColumns (moved from @zeroship/db/schema)
// ---------------------------------------------------------------------------

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/** Input form: a record of TypeBuilder instances. Field values must be
 *  produced by the `t.*` API (`t.string()`, `t.number()`, etc.). */
type SchemaFieldRecord = Record<string, TypeBuilder<unknown, boolean, any, any>>;

/** Accepts either a record of fields OR a top-level union TypeBuilder. */
type SchemaInputOrUnion = SchemaFieldRecord | TypeBuilder<unknown, boolean, any, any>;

/**
 * **P7 PR 1** — SDK-side mirror of the Rust-side `SYSTEM_FIELD_NAMES`
 * constant (`crates/plugin-db/src/query.rs`). The seven names are
 * platform-managed system fields; creator schemas cannot declare
 * fields with these names. Fences at schema-declaration time so the
 * failure shows up immediately in `pnpm dev` (not at the first DB
 * call), matching the spec's "throw at app-boot time" requirement.
 *
 * Drift between this list and the Rust constant would let creators
 * declare a field the SDK accepts but the runtime refuses (or vice-
 * versa); both lists MUST be updated together.
 */
const SYSTEM_FIELD_NAMES: readonly string[] = Object.freeze([
  "id",
  "created_at",
  "updated_at",
  "created_by",
  "updated_by",
  "version",
  "deleted_at",
]);

/**
 * Converts a SchemaInput into a NormalizedSchema. Every field value
 * must be a `TypeBuilder` produced by the `t.*` API (or the whole
 * input may be a single top-level `t.union(...)` — proposal §C2).
 * Any other shape throws.
 *
 * **P7 PR 1** — refuses any field whose name collides with a
 * platform system field. The Rust-side `field_to_column` would also
 * refuse such schemas at register-model time; throwing here lets
 * `pnpm dev` surface the error immediately on first build instead
 * of waiting for the worker round-trip.
 */
export function normalizeSchema(input: SchemaInputOrUnion): NormalizedSchema {
  // C2 — top-level discriminated union.
  if (input instanceof TypeBuilder) {
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

  for (const [key, rawVal] of Object.entries(input)) {
    // **P7 PR 1** — refuse creator-declared fields whose names collide
    // with the seven platform system fields. The Rust-side validator
    // (`validate_field_name_for_declaration`) enforces the same fence
    // at register-model; the SDK-side check surfaces the error at
    // `pnpm dev` build time so creators don't wait for a worker
    // round-trip. Error code mirrors the Rust-side
    // `RESERVED_SYSTEM_FIELD_NAME`.
    if (SYSTEM_FIELD_NAMES.includes(key)) {
      throw Object.assign(
        new Error(
          `Field name "${key}" is reserved for platform system fields. ` +
            `System fields (${SYSTEM_FIELD_NAMES.join(", ")}) are managed by ` +
            `the platform and cannot be overridden.`,
        ),
        { code: "RESERVED_SYSTEM_FIELD_NAME" as const },
      );
    }
    if (!(rawVal instanceof TypeBuilder)) {
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
 * (proposal §C2). See `docs/proposals/zeroship-db.md` for the full
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
  const variants = def.variants;
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
    variants: variants.map((v) => {
      const cloned: Record<string, FieldDef> = {};
      for (const [k, fd] of Object.entries(v)) cloned[k] = { ...fd };
      return cloned;
    }),
  };

  for (const variant of variants) {
    for (const [field, fd] of Object.entries(variant)) {
      if (field === discriminator) continue;
      const existing = result[field];
      if (existing === undefined) {
        const expanded: FieldDef = { ...fd, required: false };
        result[field] = expanded;
      } else {
        if (existing.type !== fd.type) {
          throw Object.assign(
            new Error(
              `expandUnionToFlatColumns: field "${field}" has incompatible types across variants ("${existing.type}" vs "${fd.type}")`,
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
 * `Tables<S>` constraint. Cross-app refs are also blocked.
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
    if (rawSchema instanceof TypeBuilder) {
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
      rawSchema instanceof SchemaBuilder
        ? (rawSchema as SchemaBuilder<Record<string, unknown>>).fields
        : rawSchema;
    if (fields === null || typeof fields !== "object") continue;
    for (const [field, def] of Object.entries(fields as PlainObject)) {
      if (def instanceof TypeBuilder) {
        walkFieldDef(collectionName, field, def.toFieldDef());
      } else if (
        def !== null &&
        typeof def === "object" &&
        "type" in (def as PlainObject)
      ) {
        walkFieldDef(collectionName, field, def as unknown as FieldDef);
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
 * return value too; the standalone `model()` shape kept on
 * `@zeroship/db` would re-introduce the eager-registerModel race that
 * Stage 5 fixed. If a user needs a one-off Collection outside of
 * `installSchema`, they should declare schema on the entry and read
 * `env.db.<name>` — that's the supported path.
 *
 * @internal — `installSchema` sets `skipRegister=true` so `model()`
 *   doesn't eagerly fire `registerModel` itself; `installSchema` runs
 *   the registrations in topological order.
 */
export function model<S extends Record<string, unknown>>(
  name: string,
  schema: S,
  native: NativeDb,
  namingStrategy: NamingStrategy = naming.snakeCase,
  softDelete: boolean = false,
  versioning: boolean = false,
  skipRegister: boolean = false,
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

  // When soft delete is enabled, inject the deletedAt field into the schema
  if (softDelete && !normalized.deletedAt) {
    normalized.deletedAt = { type: "date", required: false };
  }

  // D4 — when versioning is enabled, inject a `version` column.
  if (versioning && !normalized.version) {
    normalized.version = { type: "number", required: false, default: 1 };
  }

  // Register model with the runtime — creates table + columns if not exists.
  // Convert schema keys to column names for DDL.
  const dbSchema: ZeroshipDbSchema = {};
  for (const [key, def] of Object.entries(normalized)) {
    dbSchema[namingStrategy.toColumn(key)] = def as ZeroshipDbFieldDef;
  }

  const wireIndexes: ZeroshipDbNamedIndex[] = declaredIndexes.map((idx) => ({
    name: idx.name,
    fields: idx.fields.map((f) => namingStrategy.toColumn(f)),
    ...(idx.unique ? { unique: true } : {}),
  }));

  // Call via `.call(native, ...)` so the v8_class brand check sees the
  // right receiver. The unbound-fn form drops `this` and triggers
  // "Illegal invocation" — see commit e564c010 for context.
  //
  // **P9 PR 4** — `registerModel` moved off the published `ZeroshipDb`
  // surface to the `__platform` handle, so it's no longer a typed member
  // of `native: NativeDb`. The standalone `model()` path (skipRegister =
  // false) is exercised only by `@zeroship/db` unit tests, which pass a
  // mock `native` carrying its own `registerModel`; we read it via a
  // structural cast so those callers keep working. The production
  // `installSchema` path passes `skipRegister = true` and runs
  // registration through the `__platform` handle instead (see
  // `_installSchemaInner`).
  const nativeRegisterModel = (native as unknown as {
    registerModel?: (
      this: typeof native,
      collection: string,
      schema: ZeroshipDbSchema,
      indexes?: ZeroshipDbNamedIndex[],
    ) => Promise<void>;
  }).registerModel;
  let registrationPromise: Promise<void> | null = null;
  if (!skipRegister && typeof nativeRegisterModel === "function") {
    registrationPromise = nativeRegisterModel.call(native, name, dbSchema, wireIndexes);
  }

  return new Collection<S>(name, normalized, native, {
    naming: namingStrategy,
    ready: registrationPromise,
    softDelete,
    versioning,
    indexes: declaredIndexes,
  });
}

// ---------------------------------------------------------------------------
// topoSortByRefs
// ---------------------------------------------------------------------------

/**
 * Topologically sort schema names so parents precede children. A child
 * is a collection with `t.ref(parent)` somewhere in its field set.
 * Used by `installSchema` to chain `registerModel` calls in dependency
 * order. Cycles (mutual refs) fall back to declaration order — they're
 * resolved by `DEFERRABLE INITIALLY DEFERRED` at the SQL layer.
 */
function topoSortByRefs(schemas: Record<string, unknown>): string[] {
  const names = Object.keys(schemas);
  const deps = new Map<string, Set<string>>();
  for (const name of names) {
    deps.set(name, new Set());
    const raw = schemas[name];
    const fields = (raw instanceof SchemaBuilder ? raw.fields : raw) as Record<string, unknown> | unknown;
    if (!fields || typeof fields !== "object") continue;
    for (const fd of Object.values(fields as Record<string, unknown>)) {
      const def = fd instanceof TypeBuilder ? fd.toFieldDef() : (fd as { type?: string; refTarget?: string });
      if (def && (def as { type?: string }).type === "ref") {
        const target = (def as { refTarget?: string }).refTarget;
        if (target && target !== name && names.includes(target)) {
          deps.get(name)!.add(target);
        }
      }
    }
  }
  const visited = new Set<string>();
  const onStack = new Set<string>();
  const out: string[] = [];
  function visit(n: string): void {
    if (visited.has(n)) return;
    if (onStack.has(n)) return; // cycle — break; DEFERRABLE handles it
    onStack.add(n);
    for (const d of deps.get(n)!) visit(d);
    onStack.delete(n);
    visited.add(n);
    out.push(n);
  }
  for (const n of names) visit(n);
  return out;
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
  | TypeBuilder<unknown, any, any, any>;

/**
 * Schema-shape validator. When a user writes `{ name: "string" }`
 * instead of `{ name: t.string() }`, the bare value would otherwise
 * pass the `Record<string, unknown>` constraint silently. This
 * validator walks each collection's field map and emits a
 * string-literal error type at the offending field.
 */
type IsValidSchemaField<F> = F extends TypeBuilder<unknown, boolean, any, any> ? true : false;

export type ValidateSchemaShape<T> = {
  [K in keyof T]:
    T[K] extends SchemaBuilder<Record<string, unknown>>
      ? T[K]
      : T[K] extends TypeBuilder<unknown, boolean, any, any>
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
  upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }): Promise<Row<S>>;
  update(idOrFilter: string | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
  delete(idOrFilter: string | Filter<S>): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>): Promise<{ deletedCount: number }>;
  count(filter?: Filter<S>): Promise<number>;
  distinct(field: string & keyof Row<S>, filter?: Filter<S>): Promise<(string | number | boolean | null)[]>;
  aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<PlainObject[]>;
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
  T extends TypeBuilder<infer U, any, any, any> ? U :
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

let _prevChain: Promise<void> | null = null;

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
    async upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }) {
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
    async count(filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.count(filter));
    },
    async distinct(field: string & keyof Row<S>, filter: Filter<S> = {} as Filter<S>) {
      return unwrap(await collection.distinct(field, filter));
    },
    async aggregate(pipeline: ZeroshipDbAggregateStage[]) {
      return unwrap(await collection.aggregate(pipeline));
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
  /** Column naming strategy. Default: `naming.snakeCase`. */
  naming?: NamingStrategy;
  /**
   * **P9 PR 4** — the platform capability handle the caller already
   * resolved via the runtime's `globalThis.__zsDbPlatform(env.db)`
   * resolver (§8). The production `runtime-entry` resolves it once and
   * passes it here so registration (`registerModel` / `setMaskPolicy`)
   * routes through `__platform` even after the runtime-entry deletes the
   * resolver global.
   *
   * When omitted, `installSchema` resolves the handle itself; when no
   * handle is available (no DbPlugin, or a mock `env`), registration
   * falls back to a `registerModel` method on `env` directly if present
   * (the shape `@zeroship/db` unit-test mocks use), else skips.
   */
  platform?: DbPlatformHandle;
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
  "registerModel",
  "startReplicationConsumer",
  "transaction",
  "live",
]);

let _installInFlight = false;

/**
 * Framework-internal helper that backs the `export default { schema }`
 * convention. Returns `{ collections, ready }` — callers MUST await
 * `ready` before opening any auto-tx (the dispatcher does this for them).
 */
export function installSchema<const T extends Record<string, SchemaInput>>(
  schemas: ValidateSchemaShape<T>,
  env: NativeDb,
  options?: InstallSchemaOptions,
): { collections: Collections<T>; ready: Promise<void> } {
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
  } catch (e) {
    const prev = _prevChain ?? Promise.resolve();
    const reason = e instanceof Error ? e : new Error(String(e));
    const published = prev.catch(() => undefined).then(() => Promise.reject(reason));
    published.catch(() => undefined);
    _prevChain = published;
    throw e;
  } finally {
    _installInFlight = false;
  }
}

function _installSchemaInner<const T extends Record<string, SchemaInput>>(
  schemas: T,
  env: NativeDb,
  options?: InstallSchemaOptions,
): { collections: Collections<T>; ready: Promise<void> } {
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
  const namingStrategy = options?.naming ?? naming.snakeCase;
  const collections = {} as { [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T> };

  // **P9 PR 4** — registration (`registerModel`) target. In production
  // `registerModel` lives on the `__platform` capability handle, not on
  // `env.db`; resolve it via the runtime resolver (or the handle the
  // caller pre-resolved through `options.platform`). When no handle is
  // available, fall back to a `registerModel` method on `env` directly —
  // the shape `@zeroship/db`'s unit-test mocks use — else registration
  // is skipped (RPC-only / fetch-only apps, dev without a DB URL). The
  // call is dispatched with `.call(registerTarget, ...)` so the v8_class
  // brand check sees the right receiver.
  const platform = resolveDbPlatform(native, options?.platform);
  const registerTarget = (platform ?? (native as unknown)) as {
    registerModel?: (
      this: unknown,
      collection: string,
      schema: ZeroshipDbSchema,
      indexes?: ZeroshipDbNamedIndex[],
    ) => Promise<void>;
  };

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
  const nativeHolder = native as unknown as {
    [NATIVE_TX_KEY]?: (
      callback: (rawTxView: unknown) => unknown,
      opts?: { isolationLevel?: string },
    ) => Promise<unknown>;
    transaction?: unknown;
  };
  if (nativeHolder[NATIVE_TX_KEY] === undefined && typeof nativeHolder.transaction === "function") {
    const captured = (nativeHolder.transaction as (
      callback: (rawTxView: unknown) => unknown,
      opts?: { isolationLevel?: string },
    ) => Promise<unknown>).bind(native);
    Object.defineProperty(native, NATIVE_TX_KEY, {
      value: captured,
      configurable: true,
      enumerable: false,
      writable: true,
    });
  }
  const nativeTransaction = nativeHolder[NATIVE_TX_KEY];

  validateRefTargets(schemas);

  for (const [name, rawSchema] of Object.entries(schemas)) {
    const isBuilder = rawSchema instanceof SchemaBuilder;
    const fields = isBuilder ? rawSchema.fields : rawSchema;
    const softDelete = isBuilder ? rawSchema.options.softDelete : false;
    const versioning = isBuilder ? rawSchema.options.versioning : false;
    const declaredIndexes = isBuilder ? rawSchema.indexes : [];
    (collections as Record<string, Collection<unknown, string, T>>)[name] =
      model(
        name,
        fields as Record<string, unknown>,
        native,
        namingStrategy,
        softDelete,
        versioning,
        /* skipRegister */ true,
        declaredIndexes,
      ) as Collection<unknown, string, T>;
  }

  const refOrder = topoSortByRefs(schemas as Record<string, unknown>);
  let chain: Promise<void> = Promise.resolve();
  for (const name of refOrder) {
    const col = (collections as Record<string, Collection<unknown, string, T>>)[name];
    if (!col) continue;
    const rawSchema = schemas[name as keyof T];
    const fields =
      rawSchema instanceof SchemaBuilder ? rawSchema.fields : rawSchema;
    const normalized = normalizeSchema(fields as Parameters<typeof normalizeSchema>[0]);
    const dbSchema: ZeroshipDbSchema = {};
    for (const [key, def] of Object.entries(normalized)) {
      dbSchema[namingStrategy.toColumn(key)] = def as ZeroshipDbFieldDef;
    }
    const declaredIndexes =
      rawSchema instanceof SchemaBuilder ? rawSchema.indexes : [];
    const wireIndexes: ZeroshipDbNamedIndex[] = declaredIndexes.map((idx) => ({
      name: idx.name,
      fields: idx.fields.map((f) => namingStrategy.toColumn(f)),
      ...(idx.unique ? { unique: true } : {}),
    }));
    chain = chain.then(() => {
      // **P9 PR 4** — register through the `__platform` handle (or the
      // mock fallback); see `registerTarget` above.
      if (typeof registerTarget.registerModel !== "function") {
        return Promise.resolve();
      }
      return registerTarget.registerModel.call(
        registerTarget,
        name,
        dbSchema,
        wireIndexes,
      );
    });
    (col as unknown as { _setReady(p: Promise<void> | null): void })._setReady(chain);
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
  // `crates/plugin-db/src/orchestrator/transaction.rs`). It calls
  // `callback(rawTxView)` once BEGIN/SAVEPOINT succeeds and returns a
  // promise that resolves with the callback's result on commit (callback
  // resolved) or rejects with the callback's error on rollback (callback
  // threw). The four observable error codes — `BEGIN_FAILED`,
  // `COMMIT_FAILED_INDETERMINATE`, `savepoint_depth_exceeded`, and the
  // body-error passthrough — are emitted by Rust and surface verbatim on
  // the rejection (`err.code`).
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

    const collectionList = Object.values(collections).map(
      (c) => c as unknown as {
        _txDepth: number;
        _idLoader: { _drain(): Promise<void> } | null;
      },
    );

    // 1. Drain the JS DataLoader queues (JS-only state; cannot move to
    //    Rust). A drain failure aborts before any BEGIN runs.
    try {
      await Promise.all(
        collectionList
          .map((c) => c._idLoader?._drain())
          .filter((p): p is Promise<void> => p !== undefined),
      );
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
    for (const c of collectionList) c._txDepth += 1;
    try {
      // 3. Native orchestrator: begin → callback(txCollections) →
      //    commit/rollback. Resolves with the callback's result on
      //    commit; rejects (with the typed `.code`) on rollback /
      //    begin-failed / commit-indeterminate / depth-exceeded.
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
      // (`COMMIT_FAILED_INDETERMINATE` / `BEGIN_FAILED` /
      // `savepoint_depth_exceeded`) or is the creator's own thrown error
      // verbatim. Surface it as `result.error`.
      return err(txErr instanceof Error ? txErr : new Error(String(txErr)));
    } finally {
      for (const c of collectionList) c._txDepth -= 1;
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

  const prev = _prevChain ?? Promise.resolve();
  const ready = prev.catch(() => undefined).then(() => chain);
  _prevChain = ready;
  ready.catch(() => undefined);

  return { collections: collections as Collections<T>, ready };
}
