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
// normalizeSchema + expandUnionToFlatColumns (moved from @zeroship/db/schema)
// ---------------------------------------------------------------------------

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/** Input form: a record of TypeBuilder instances. Field values must be
 *  produced by the `t.*` API (`t.string()`, `t.number()`, etc.). */
type SchemaFieldRecord = Record<string, TypeBuilder<unknown, boolean>>;

/** Accepts either a record of fields OR a top-level union TypeBuilder. */
type SchemaInputOrUnion = SchemaFieldRecord | TypeBuilder<unknown, boolean>;

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
      { code: "schema_top_level_not_union" as const },
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
    // `reserved_system_field_name`.
    if (SYSTEM_FIELD_NAMES.includes(key)) {
      throw Object.assign(
        new Error(
          `Field name "${key}" is reserved for platform system fields. ` +
            `System fields (${SYSTEM_FIELD_NAMES.join(", ")}) are managed by ` +
            `the platform and cannot be overridden.`,
        ),
        { code: "reserved_system_field_name" as const },
      );
    }
    if (!(rawVal instanceof TypeBuilder)) {
      throw Object.assign(
        new Error(
          `unrecognized schema field "${key}": every field must be a t.* builder ` +
            `(e.g. t.string(), t.number(), t.ref("users")). Bare constructors and ` +
            `Mongoose-style { type: Constructor } objects are no longer supported.`,
        ),
        { code: "schema_field_not_typebuilder" as const },
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
      { code: "union_expand_not_union" as const },
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
        { code: "union_variant_missing_discriminator" as const },
      );
    }
    const lit = fd.literalValue;
    const primTy = typeof lit;
    if (primTy !== "string" && primTy !== "number" && primTy !== "boolean") {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: discriminator literal of variant #${i} has unsupported type "${primTy}"`,
        ),
        { code: "union_discriminator_unsupported_type" as const },
      );
    }
    if (discPrimType === null) {
      discPrimType = primTy as "string" | "number" | "boolean";
    } else if (discPrimType !== primTy) {
      throw Object.assign(
        new Error(
          `expandUnionToFlatColumns: discriminator literals across variants must share a primitive type (got "${discPrimType}" and "${primTy}")`,
        ),
        { code: "union_discriminator_type_mismatch" as const },
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
        { code: "union_duplicate_discriminator_value" as const },
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
            { code: "union_field_type_mismatch" as const },
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
 * with `code = "ref_target_not_found"` on the first violation.
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
      code: "ref_target_not_found",
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
      { code: "model_invalid_name" as const },
    );
  }
  if (schema === null || schema === undefined || typeof schema !== "object") {
    throw Object.assign(
      new Error("model schema must be an object"),
      { code: "model_invalid_schema" as const },
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
  let registrationPromise: Promise<void> | null = null;
  if (!skipRegister && native.registerModel) {
    registrationPromise = (native.registerModel as unknown as (
      this: typeof native,
      collection: string,
      schema: ZeroshipDbSchema,
      indexes?: ZeroshipDbNamedIndex[],
    ) => Promise<void>).call(native, name, dbSchema, wireIndexes);
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
  | TypeBuilder<unknown, any>;

/**
 * Schema-shape validator. When a user writes `{ name: "string" }`
 * instead of `{ name: t.string() }`, the bare value would otherwise
 * pass the `Record<string, unknown>` constraint silently. This
 * validator walks each collection's field map and emits a
 * string-literal error type at the offending field.
 */
type IsValidSchemaField<F> = F extends TypeBuilder<unknown, boolean> ? true : false;

export type ValidateSchemaShape<T> = {
  [K in keyof T]:
    T[K] extends SchemaBuilder<Record<string, unknown>>
      ? T[K]
      : T[K] extends TypeBuilder<unknown, boolean>
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
    idOrFilter: number | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Pick<Row<S>, K> | null>;
  get<W extends WithSpec>(
    idOrFilter: number | Filter<S>,
    opts: { with: W; orderBy?: Record<string, 1 | -1> },
  ): Promise<(Row<S> & WithRelations<S, W, AllSchemas>) | null>;
  get(
    idOrFilter: number | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Row<S> | null>;
  exists(filter: Filter<S>): Promise<boolean>;
  find<W extends WithSpec>(filter: Filter<S>, opts: { with: W }): TxQuery<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): TxQuery<S, Row<S>, AllSchemas>;
  upsert(row: RowInput<S>, options: { conflictFields: (string & keyof Row<S>)[] }): Promise<Row<S>>;
  update(idOrFilter: number | Filter<S>, patch: UpdateExpression<S>): Promise<Row<S> | null>;
  updateMany(filter: Filter<S>, patch: UpdateExpression<S>): Promise<{ count: number }>;
  delete(idOrFilter: number | Filter<S>, opts?: { hard?: boolean }): Promise<Row<S> | null>;
  deleteMany(filter: Filter<S>, opts?: { hard?: boolean }): Promise<{ deletedCount: number }>;
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
  after(id: number): TxQuery<S, P, AllSchemas>;
  with<W extends WithSpec>(spec: W): TxQuery<S, P & WithRelations<S, W, AllSchemas>, AllSchemas>;
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
  T extends TypeBuilder<infer U, any> ? U :
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
    idOrFilter: number | Filter<S>,
    opts?: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<unknown> {
    const colAny = collection as unknown as {
      get(
        idOrFilter: number | Filter<S>,
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
    async update(idOrFilter: number | Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.update(idOrFilter, patch));
    },
    async updateMany(filter: Filter<S>, patch: UpdateExpression<S>) {
      return unwrap(await collection.updateMany(filter, patch));
    },
    async delete(idOrFilter: number | Filter<S>, opts?: { hard?: boolean }) {
      return unwrap(await collection.delete(idOrFilter, opts));
    },
    async deleteMany(filter: Filter<S>, opts?: { hard?: boolean }) {
      return unwrap(await collection.deleteMany(filter, opts));
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
    after(id: number) { query.after(id); return wrapped; },
    with: ((spec: WithSpec) => {
      (query as unknown as { with(s: WithSpec): unknown }).with(spec);
      return wrapped;
    }) as TxQuery<S, Row<S>>["with"],
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
}

const RESERVED_ENV_DB_NAMES = new Set<string>([
  "collection",
  "migrations",
  "replication",
  "beginTransaction",
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
      { code: "install_in_flight" as const },
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
      { code: "native_db_unavailable" as const },
    );
  }
  const native = env;
  const namingStrategy = options?.naming ?? naming.snakeCase;
  const collections = {} as { [K in keyof T]: Collection<UnwrapSchema<T[K]>, K & string, T> };

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
      if (!native.registerModel) return Promise.resolve();
      return (native.registerModel as unknown as (
        this: typeof native,
        collection: string,
        schema: ZeroshipDbSchema,
        indexes?: ZeroshipDbNamedIndex[],
      ) => Promise<void>).call(native, name, dbSchema, wireIndexes);
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

  async function transactionImpl<R>(
    fn: (tx: { [K in keyof T]: TxCollection<UnwrapSchema<T[K]>, T> }) => Promise<R>,
    txOptions?: TransactionOptions,
  ): Promise<Result<R>> {
    const nativeAny = native as unknown as {
      beginTransaction?: (opts?: { isolationLevel?: string }) => Promise<{
        commit(): Promise<void>;
        rollback(): Promise<void>;
      }>;
    };
    if (typeof nativeAny.beginTransaction !== "function") {
      return err(
        Object.assign(
          new Error(
            "@zeroship/bootstrap: env.db.beginTransaction not available — " +
              "runtime is missing the Transaction v8_class surface.",
          ),
          { code: "native_transaction_unavailable" as const },
        ),
      );
    }
    const collectionList = Object.values(collections).map(
      (c) => c as unknown as {
        _txDepth: number;
        _idLoader: { _drain(): Promise<void> } | null;
      },
    );
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
        { code: "tx_drain_failed" as const },
      );
      return err(wrapped);
    }

    for (const c of collectionList) {
      c._txDepth += 1;
    }
    let tx: { commit(): Promise<void>; rollback(): Promise<void> };
    try {
      tx = await nativeAny.beginTransaction(
        txOptions?.isolationLevel ? { isolationLevel: txOptions.isolationLevel } : undefined,
      );
    } catch (beginErr) {
      for (const c of collectionList) c._txDepth -= 1;
      const baseErr =
        beginErr instanceof Error ? beginErr : new Error(String(beginErr));
      const finalErr =
        typeof (baseErr as Error & { code?: unknown }).code === "string"
          ? baseErr
          : Object.assign(baseErr, { code: "begin_failed" as const });
      return err(finalErr);
    }

    let bodyResult: R;
    try {
      bodyResult = await fn(txCollections);
    } catch (bodyErr) {
      try { await tx.rollback(); } catch { /* rollback failure tolerated */ }
      for (const c of collectionList) c._txDepth -= 1;
      return err(bodyErr instanceof Error ? bodyErr : new Error(String(bodyErr)));
    }
    try {
      await tx.commit();
    } catch (commitErr) {
      try { await tx.rollback(); } catch { /* commit-half may make rollback a no-op */ }
      for (const c of collectionList) c._txDepth -= 1;
      const msg = commitErr instanceof Error ? commitErr.message : String(commitErr);
      const wrapped = Object.assign(
        new Error(`commit failed — transaction state indeterminate: ${msg}`, {
          cause: commitErr instanceof Error ? commitErr : undefined,
        }),
        { code: "commit_failed_indeterminate" as const },
      );
      return err(wrapped);
    }
    for (const c of collectionList) c._txDepth -= 1;
    return ok(bodyResult);
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
          { code: "reserved_env_db_name" as const },
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
