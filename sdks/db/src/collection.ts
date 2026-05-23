/**
 * Collection: the main entry point for CRUD operations on a named collection.
 * Each method validates inputs against the schema, maps field names to the native
 * format, calls the native driver, and maps results back to the user-facing shape.
 */
import { NormalizedSchema } from "./schema.js";
import { validateDoc, checkPartial, isValidCalendarDate, isJsonSerializable, isParseableDateString } from "./validate.js";
import { mapNativeError, ValidationError, OptimisticLockError } from "./errors.js";
import {
  mapResultDoc,
  mapDocOutbound,
  mapFilterOutbound,
  mapUpdateOutbound,
  translateAggregatePipeline,
} from "./utils.js";
import { Query } from "./query.js";
import { IdLoader } from "./loader.js";
import { trackCollectionAccess } from "./live.js";
import { PlainObject, Result, Row, RowInput, UpdateExpression, Filter, type Id, type NamingStrategy, type NamedIndexSpec, type WithSpec, type WithRelations, naming, ok, err } from "./types.js";

/** The native driver interface from @zeroship/types. */
export type NativeDb = ZeroshipDb;

/** The native Collection wrapper from @zeroship/types. */
export type NativeCollection = ZeroshipCollection;

/**
 * Validates `k` / `limit` arguments to `.search()` are positive integers
 * in `1..=1000`. Throws ValidationError with `code: "invalid_k"` on
 * violation. The 1000-row ceiling matches the engine-side practical
 * limit for kNN flat scan + GIN/ivfflat result sets.
 */
function _validateK(value: number, paramName: string): void {
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 1 ||
    value > 1000
  ) {
    throw new ValidationError({
      args: {
        path: paramName,
        message: `search: \`${paramName}\` must be an integer in 1..=1000 (got ${value})`,
      },
    });
  }
}

/**
 * Converts a caught value to an Error for inclusion in a Result.
 * ValidationError instances are returned as-is (they are already well-typed).
 * All other errors are passed through mapNativeError so that, e.g., unique
 * constraint violations receive code 11000.
 */
function toResultError(e: unknown): Error {
  let out: Error;
  if (e instanceof ValidationError) out = e;
  else if (e instanceof OptimisticLockError) out = e;
  // Pass the whole value through mapNativeError — when the native side
  // already threw an Error with a `.code` (e.g. "migration_already_running"),
  // mapNativeError returns it untouched so the structured code reaches the
  // caller via `result.error.code`.
  else out = mapNativeError(e);
  // Errors serialize to `{}` by default (message/name are
  // non-enumerable). Attach `toJSON` so the RPC wire
  // (`JSON.stringify({ data, error })`) preserves message + code
  // instead of dropping them.
  try {
    Object.defineProperty(out, "toJSON", {
      value: function () {
        const obj: Record<string, unknown> = {
          name: (this as Error).name,
          message: (this as Error).message,
        };
        const code = (this as { code?: unknown }).code;
        if (code !== undefined) obj.code = code;
        const errs = (this as { errors?: unknown }).errors;
        if (errs !== undefined) obj.errors = errs;
        return obj;
      },
      enumerable: false,
      configurable: true,
      writable: true,
    });
  } catch {
    /* frozen error — no-op */
  }
  return out;
}

/**
 * Extracts the plain field map from an update argument for validation.
 * Handles both `{ $set: { field: val } }` and bare `{ field: val }` styles.
 * `$push`, `$addToSet`, `$inc`, `$dec`, `$mul`, and other operators are excluded.
 */
function extractUpdateFields(update: PlainObject): PlainObject {
  const fields: PlainObject = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      for (const k of Object.keys(val as PlainObject)) {
        if (k === "__proto__" || k === "constructor" || k === "prototype") continue;
        fields[k] = (val as PlainObject)[k];
      }
    } else if (!key.startsWith("$")) {
      // Skip per-field operator objects like { $inc: 1 } — they are not plain values
      if (typeof val === "object" && val !== null && !Array.isArray(val) &&
          Object.keys(val as PlainObject).every(k => k.startsWith("$"))) continue;
      fields[key] = val;
    }
  }
  return fields;
}

/**
 * Validates $push / $addToSet values against the schema's array item type.
 * Throws ValidationError if any pushed value does not match the declared items type.
 * Numeric operators ($inc, $dec, $mul) are skipped — they are inherently numeric.
 *
 * Exported for in-process regression tests (see `r5-array-item-validation.test.ts`).
 * Production callers go through the collection update path.
 */
export function validateArrayPushOps(
  update: PlainObject,
  schema: NormalizedSchema
): void {
  for (const op of ["$push", "$addToSet"] as const) {
    const opVal = update[op];
    if (opVal === null || typeof opVal !== "object") continue;

    for (const [field, val] of Object.entries(opVal as PlainObject)) {
      const def = schema[field];
      if (!def || def.type !== "array" || !def.items) continue;
      const itemType = def.items;

      // R6 — keep this branch list in sync with the array-item branch in
      // `validate.ts` (`checkField`'s `type === "array"` block) and with
      // `PRIMITIVE_ITEM_TYPES` in `types.ts`. Adding a new primitive item
      // type means a case in all three places.
      let valid = true;
      if (itemType === "string") valid = typeof val === "string";
      else if (itemType === "number") valid = typeof val === "number";
      else if (itemType === "boolean") valid = typeof val === "boolean";
      else if (itemType === "date") valid = val instanceof Date || (typeof val === "string" && isParseableDateString(val));
      else if (itemType === "calendarDate") valid = typeof val === "string" && isValidCalendarDate(val);
      else if (itemType === "json") valid = isJsonSerializable(val);

      if (!valid) {
        throw new ValidationError({
          [field]: {
            path: field,
            message: `${op} value for ${field} must be a ${itemType}`,
          },
        });
      }
    }
  }
}

/**
 * D1 — set of `${collection}:${sortedFilterKeys}` shapes already warned
 * about. Module-scope so a single warning fires per shape across all
 * Collection instances in the same isolate. Reset between tests by
 * accessing `__zeroshipDbResetIndexWarnings()`.
 *
 * Bounded LRU: a `Map<string, true>` whose insertion-order iteration is
 * guaranteed by the JS spec. On overflow we evict the oldest entry — an
 * AI-generated app that synthesises new filter shapes (metric names,
 * dynamic identifiers) over a long-lived dev server would otherwise leak
 * one entry per shape forever (Gap P). 1024 is generous for any real
 * app and bounds the memory hard.
 */
const MAX_WARNED_SHAPES = 1024;
const _warnedShapes: Map<string, true> = new Map();

/**
 * D1 — record a fired warning for `key`. Returns `true` iff this is the
 * first time we've seen the shape (caller should fire the warning).
 * Evicts the oldest entry once `MAX_WARNED_SHAPES` is hit.
 */
function _noteWarnedShape(key: string): boolean {
  if (_warnedShapes.has(key)) return false;
  if (_warnedShapes.size >= MAX_WARNED_SHAPES) {
    // Evict oldest insertion (Map keys() iterates in insertion order).
    const oldest = _warnedShapes.keys().next().value;
    if (oldest !== undefined) _warnedShapes.delete(oldest);
  }
  _warnedShapes.set(key, true);
  return true;
}

/** @internal — test-only reset hook. Not part of the public API. */
export function __zeroshipDbResetIndexWarnings(): void {
  _warnedShapes.clear();
}

/** @internal — test-only size accessor. Not part of the public API. */
export function __zeroshipDbWarnedShapesSize(): number {
  return _warnedShapes.size;
}

/**
 * D1 — emit a one-time `console.warn` if `filter` would do a sequential
 * scan because no declared index covers its keys. Coverage rule: an
 * index `{name, fields: [f1, f2, ...]}` covers the filter when the
 * filter's key set is a non-empty prefix of `fields` (Postgres can use
 * a multi-column B-tree for any leftmost-prefix subset). Single-field
 * `.unique()` / `.index()` markers are still recognised — they desugar
 * to a single-column index. Only fires when `process.env.NODE_ENV !==
 * "production"`. Deduplicates by `${collection}:${sortedKeys}`.
 */
function _maybeWarnUnindexedFilter(
  collection: string,
  schema: NormalizedSchema,
  filter: PlainObject,
  declaredIndexes: readonly NamedIndexSpec[],
): void {
  // Avoid the work in production AND test. NODE_ENV is set to "test" by
  // most JS test runners (vitest/jest set it automatically; node:test
  // users typically set it explicitly via `NODE_ENV=test npm test`).
  // Skipping in test keeps mock-based suites quiet without disabling the
  // warning where it matters (dev: NODE_ENV unset or "development").
  const nodeEnv = (globalThis as { process?: { env?: { NODE_ENV?: string } } }).process?.env?.NODE_ENV;
  if (nodeEnv === "production") return;
  // In tests we honour an explicit opt-in so the named-indexes suite can
  // observe the warning without forcing every other suite to deal with it.
  const opt = (globalThis as { __zeroshipDbWarnIndexInTest?: boolean }).__zeroshipDbWarnIndexInTest;
  if (nodeEnv === "test" && opt !== true) return;

  if (filter === null || typeof filter !== "object") return;
  const keys = Object.keys(filter).filter(
    (k) => !k.startsWith("$") && k in schema,
  );
  if (keys.length === 0) return;
  // `id` is always the primary key — never warn on it.
  if (keys.length === 1 && keys[0] === "id") return;

  if (_filterCoveredByIndex(keys, schema, declaredIndexes)) return;

  const shapeKey = `${collection}:${[...keys].sort().join(",")}`;
  if (!_noteWarnedShape(shapeKey)) return;

  const declaredNames = declaredIndexes.map((i) => i.name);
  const declaredHint = declaredNames.length > 0
    ? `Declared indexes: ${declaredNames.join(", ")}.`
    : "No indexes declared on this collection.";
  // `console.warn` is the standard channel here — matches Convex's
  // ESLint rule shape. We do not throw: this is a nudge, not a hard error.
  console.warn(
    `[@zeroship/db] unindexed query on "${collection}" — ` +
    `filter keys [${keys.join(", ")}] match no declared index. ` +
    `${declaredHint} ` +
    `Add .index("by_X", [${keys.map((k) => JSON.stringify(k)).join(", ")}]) ` +
    `to the schema, or filter by a prefix of an existing index.`,
  );
}

/**
 * True iff the filter keys are covered by an index:
 *
 *   - Single-key filter: the one key carries a single-field marker
 *     (`def.index === true` / `def.unique === true`) OR the key set
 *     forms a non-empty leftmost prefix of some declared multi-column
 *     index.
 *   - Multi-key filter: the keys form a non-empty leftmost prefix of
 *     some declared multi-column index, OR every key carries its own
 *     single-field marker.
 *
 * Round-1 critique #3: the prior rule was "single-key OR (compound +
 * any one key marked)" — which silently hid scans like
 * `find({ userId, done })` when only `done` was `.index()`-marked,
 * even though no compound index covered `(userId, done)`. The warning
 * exists to nudge users toward declaring the right index; the rule
 * above keeps the single-field shortcut for `t.string().unique()` but
 * stops accepting "any one marked key" as compound coverage.
 */
function _filterCoveredByIndex(
  keys: string[],
  schema: NormalizedSchema,
  declaredIndexes: readonly NamedIndexSpec[],
): boolean {
  if (keys.length === 0) return false;
  // Multi-column path: keys form a leftmost prefix of some declared
  // index. Postgres can use any leftmost subset of a B-tree, so a
  // `{userId}`-only filter still hits `by_user_done = [userId, done]`.
  const keySet = new Set(keys);
  for (const idx of declaredIndexes) {
    if (idx.fields.length === 0) continue;
    if (keySet.size > idx.fields.length) continue;
    let covers = true;
    for (let i = 0; i < keySet.size; i++) {
      if (!keySet.has(idx.fields[i])) {
        covers = false;
        break;
      }
    }
    if (covers) return true;
  }
  // Single-field-marker path: either the filter is single-key on a
  // marked column, OR every key in the filter is marked. This keeps
  // `t.string().unique()` shortcutting without `.index(...)` calls
  // while refusing to call a `{userId, done}` filter "covered" just
  // because one of the two columns is marked.
  for (const k of keys) {
    const def = schema[k];
    if (!def || (def.index !== true && def.unique !== true)) return false;
  }
  return true;
}

/**
 * Represents a named collection and exposes the full CRUD + aggregate API.
 * The generic parameter `S` is the raw schema shape from which document and input
 * types are derived. `N` carries the table name as a string-literal so the
 * `Id` accessor below produces `Id<N>` rather than `Id<string>`.
 *
 * `AllSchemas` is the parent db's full schema map — threaded in by
 * `installSchema` so a `find({...}, { with: { userId: true } })` can
 * resolve the joined field's type to the target collection's `Row<...>`
 * rather than the v1 fallback of `PlainObject`. Standalone `model()`
 * callers inherit the safe default and degrade to `PlainObject` per
 * relation.
 *
 * Use `model()` or declare the schema via `export default { schema }`
 * and access through `env.db.<name>` — do not construct directly.
 */
export class Collection<
  S = PlainObject,
  N extends string = string,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;
  /** Lazily resolved Collection v8_class instance — see `_nativeCollection()`. */
  private _nativeCol: NativeCollection | null;
  private _knownFields: Set<string>;
  private _toColumn: (field: string) => string;
  private _toField: (column: string) => string;
  private _ready: Promise<void> | null;
  private _softDelete: boolean;
  private _versioning: boolean;
  /**
   * Named multi-column indexes declared via `schema(...).index(name, fields)`.
   * Field names are already mapped to column names so the runtime warning
   * compares them against filters that have also been column-mapped.
   */
  private _indexes: readonly NamedIndexSpec[];
  /** Per-collection DataLoader, lazily constructed on first batchable `get(id)`. */
  private _idLoader: IdLoader<Row<S>> | null;
  /**
   * Sibling-collection lookup, planted by `installSchema` so `with: { fk: true }`
   * can resolve `fieldDef.refTarget` → the target `Collection` to fire one
   * batched `find({id: {$in: ids}})` against. `model()` callers without a
   * parent db leave this null; `with` then errors at call time with a
   * clear message instead of silently degrading to N+1.
   */
  private _resolveCollection: ((name: string) => Collection<unknown> | undefined) | null;
  /**
   * Active-transaction depth. `db.transaction()` wraps `tx.x.*` calls
   * with an increment/decrement so the loader is bypassed while a tx is
   * live on this collection — see `_callWithTx` in db.ts. Mixing a
   * batched read with `TX_CONN`-routed reads in the same microtask
   * would otherwise blur the connection-routing boundary.
   */
  private _txDepth: number;

  /**
   * Type-only handle for `Id<TableName>` — write `typeof db.users.Id` to
   * get a branded `Id<"users">` without rebuilding the name through
   * generic argument inference. At runtime these are non-enumerable
   * `null` properties; the brand exists purely at the type layer (see
   * `Id<T>` in `./types.ts`).
   */
  declare readonly Id: Id<N>;

  /**
   * Type-only handle for `RowInput<S>` — write `typeof db.users.RowInput`
   * to receive the insert-shaped type without `Parameters<...>` plumbing.
   * Mirrors `Id` above; runtime value is `null`.
   */
  declare readonly RowInput: RowInput<S>;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb, options?: { naming?: NamingStrategy; ready?: Promise<void> | null; softDelete?: boolean; versioning?: boolean; indexes?: readonly NamedIndexSpec[] }) {
    this._name = name;
    this._schema = schema;
    this._native = native;
    this._nativeCol = null;
    this._ready = options?.ready ?? null;
    this._softDelete = options?.softDelete ?? false;
    this._versioning = options?.versioning ?? false;
    this._idLoader = null;
    this._txDepth = 0;
    this._resolveCollection = null;

    // Build field↔column lookup maps once at init — O(1) at query time
    const strategy = options?.naming ?? naming.asIs;
    const fieldToCol: Record<string, string> = {};
    const colToField: Record<string, string> = {};
    for (const field of Object.keys(schema)) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    const autoFields = ["id", "createdAt", "updatedAt"];
    if (this._softDelete) autoFields.push("deletedAt");
    for (const field of autoFields) {
      const col = strategy.toColumn(field);
      fieldToCol[field] = col;
      colToField[col] = field;
    }
    this._knownFields = new Set(Object.keys(fieldToCol));
    this._toColumn = (field) => fieldToCol[field] ?? field;
    this._toField = (column) => colToField[column] ?? column;

    // The warning path compares declared indexes against unmapped JS
    // field names (matching `_schema` keys + the filter's user-visible
    // shape). Wire-format column mapping happens once at registerModel
    // time in `model()`, not here.
    this._indexes = (options?.indexes ?? []).map((idx) => ({
      name: idx.name,
      fields: [...idx.fields],
      ...(idx.unique ? { unique: true } : {}),
    }));
  }

  /** Await table registration (DDL) before first operation. */
  private async ensureReady(): Promise<void> {
    if (this._ready) {
      await this._ready;
      this._ready = null; // Only await once
    }
  }

  /**
   * Resolve the Collection v8_class instance for this collection name.
   * Cached on first call so subsequent CRUD ops are a single property
   * read. The native runtime exposes `env.db.collection(name)` as a
   * Db v8_method that returns a typed Collection wrapper; calling it
   * twice with the same `name` returns the same JS object (identity is
   * cached on the Db wrapper).
   */
  private _nativeCollection(): NativeCollection {
    if (this._nativeCol) return this._nativeCol;
    const dbAny = this._native as unknown as { collection?: (n: string) => NativeCollection };
    if (typeof dbAny.collection !== "function") {
      throw Object.assign(
        new Error(
          "@zeroship/db: env.db.collection(name) not available — " +
          "runtime is missing the Collection v8_class surface.",
        ),
        { code: "native_collection_unavailable" as const },
      );
    }
    this._nativeCol = dbAny.collection(this._name);
    return this._nativeCol;
  }

  /** @internal — used by `installSchema` to chain registrations
   *  sequentially for B2 cross-table FK ordering. Replaces the
   *  per-collection `_ready` promise set during `model()` construction
   *  with a chained one so that parent-table registration completes
   *  before child-table registration starts. */
  _setReady(p: Promise<void> | null): void {
    this._ready = p;
  }

  /** @internal — planted by `installSchema` so the `with: { fk: true }`
   *  option can resolve sibling collections by table name. */
  _setResolveCollection(
    fn: (name: string) => Collection<unknown> | undefined,
  ): void {
    this._resolveCollection = fn;
  }

  /**
   * @internal — eager-load referenced rows for each `with` key onto every
   *  parent row. Mutates the rows in place. Used by both the `get` and
   *  `find` paths so the relation-loading logic lives in one place.
   *
   *  Per `with` key:
   *    1. Walk the schema; the key must be a `t.ref(...)` field.
   *    2. Resolve the target Collection via the planted `_resolveCollection`.
   *    3. Dedupe foreign ids across the parent rows.
   *    4. Fire ONE `find({id: {$in: [...]}})` against the target.
   *    5. Build an id→row map; the joined row replaces the FK number at
   *       the same key (null for null FK or missing target row).
   *
   *  v1 limitation: the joined row overwrites the FK number at the same
   *  key. To keep both, declare the FK on a separate field — e.g.
   *  `user: t.ref("users")` instead of `userId: t.ref("users")` — and the
   *  number lives on the joined row as `user.id`.
   */
  async _loadRelations(
    rows: PlainObject[],
    withSpec: WithSpec,
  ): Promise<void> {
    if (rows.length === 0) return;
    // Each entry mutates a DISJOINT key on the same `rows` array, so
    // running the per-relation loaders in parallel is race-safe — two
    // `with` keys (e.g. `userId` and `projectId`) used to serialise to
    // 2× latency under the old `for..of await` loop.
    await Promise.all(
      Object.entries(withSpec).map(async ([field, spec]) => {
        if (spec !== true) {
          throw Object.assign(
            new Error(
              `find/get: with: { ${field}: ${JSON.stringify(spec)} } — only \`true\` is supported in v1`,
            ),
            { code: "with_unsupported_value" as const },
          );
        }
        const fieldDef = this._schema[field];
        if (!fieldDef || fieldDef.type !== "ref") {
          throw Object.assign(
            new Error(
              `find/get: with: { ${field}: true } — "${field}" is not a t.ref field on "${this._name}"`,
            ),
            { code: "with_not_a_ref_field" as const },
          );
        }
        const targetName = fieldDef.refTarget;
        if (typeof targetName !== "string" || targetName.length === 0) {
          throw Object.assign(
            new Error(
              `find/get: with: { ${field}: true } — "${field}" has no refTarget`,
            ),
            { code: "with_missing_ref_target" as const },
          );
        }
        const resolve = this._resolveCollection;
        if (resolve === null) {
          throw Object.assign(
            new Error(
              `find/get: with: { ${field}: true } — this Collection was created via model() without a parent db, ` +
                `so sibling collections cannot be resolved. Declare the schema via "export default { schema }" to enable relation loading.`,
            ),
            { code: "with_no_parent_db" as const },
          );
        }
        const targetCol = resolve(targetName);
        if (!targetCol) {
          throw Object.assign(
            new Error(
              `find/get: with: { ${field}: true } — target collection "${targetName}" is not declared on this db`,
            ),
            { code: "with_target_not_found" as const },
          );
        }
        // Collect distinct FK values for this relation. The previous
        // `typeof v === "number"` gate silently nulled bigint / string
        // FKs; now we accept number + bigint (coerced to number for the
        // IN clause) and throw loudly on a non-numeric string so the
        // caller learns about the schema mismatch instead of seeing a
        // mysterious null in the joined field.
        //
        // bigint coercion fence: `Number(bigint)` loses precision above
        // 2^53. A precision-loss here would surface much later as
        // "wrong row joined" since the id→row map below keys on the
        // truncated number. Round-trip via `BigInt(Number(v)) === v`
        // and throw if the value can't fit losslessly — same TypeError
        // class as the non-numeric branch so callers get one code path.
        const ids: number[] = [];
        const seen = new Set<number>();
        for (const r of rows) {
          const v = r[field];
          if (v === null || v === undefined) continue;
          let n: number;
          if (typeof v === "number") {
            if (!Number.isFinite(v)) continue;
            n = v;
          } else if (typeof v === "bigint") {
            n = Number(v);
            if (BigInt(n) !== v) {
              throw Object.assign(
                new TypeError(
                  `_loadRelations: FK value for field '${field}' (${String(v)}n) exceeds Number.MAX_SAFE_INTEGER — joining would lose precision`,
                ),
                { code: "with_fk_precision_loss" as const },
              );
            }
          } else {
            throw Object.assign(
              new TypeError(
                `_loadRelations: FK value for field '${field}' is not a number-like value (got ${typeof v})`,
              ),
              { code: "with_fk_not_numeric" as const },
            );
          }
          if (!seen.has(n)) {
            seen.add(n);
            ids.push(n);
          }
        }
        if (ids.length === 0) {
          // No non-null FK values across the page — every row's relation is null.
          for (const r of rows) r[field] = null;
          return;
        }
        const { data: targetRows, error } = await targetCol.find({ id: { $in: ids } } as Filter<unknown>);
        if (error) throw error;
        const byId = new Map<number, PlainObject>();
        for (const tr of (targetRows ?? []) as PlainObject[]) {
          const tid = tr.id;
          if (typeof tid === "number") byId.set(tid, tr);
        }
        for (const r of rows) {
          const v = r[field];
          if (v === null || v === undefined) {
            r[field] = null;
          } else if (typeof v === "number" && Number.isFinite(v)) {
            r[field] = byId.get(v) ?? null;
          } else if (typeof v === "bigint") {
            r[field] = byId.get(Number(v)) ?? null;
          } else {
            // We threw above for non-numeric strings; anything reaching
            // here would be an impossible mid-iteration type flip.
            r[field] = null;
          }
        }
      }),
    );
  }

  /** Wraps an operation in ensureReady + try/catch → Result. Eliminates boilerplate per method. */
  private async _run<T>(fn: () => Promise<T>): Promise<Result<T>> {
    try {
      await this.ensureReady();
      return ok(await fn());
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * D4 — return the caller-supplied `version: N` value from a filter,
   * but only when versioning is enabled on this collection AND the
   * value is a plain number (not a `$gt`/`$in`/etc. operator). Returns
   * `null` otherwise so callers can short-circuit to the non-CAS path.
   */
  private _extractCasVersion(filter: PlainObject): number | null {
    if (!this._versioning) return null;
    if (filter === null || typeof filter !== "object") return null;
    const v = filter.version;
    if (typeof v === "number" && Number.isFinite(v)) return v;
    return null;
  }

  /**
   * D4 — when a CAS version is in play, layer `{ $inc: { version: 1 } }`
   * on top of the user-supplied update so the bump happens atomically
   * inside the same SQL statement as the SET. We merge into any
   * existing `$inc` rather than overwriting.
   */
  private _augmentUpdateWithVersion(update: PlainObject, casVersion: number | null): PlainObject {
    if (casVersion === null) return update;
    const result: PlainObject = { ...update };
    const existingInc = result.$inc;
    const inc =
      existingInc !== null && typeof existingInc === "object" && !Array.isArray(existingInc)
        ? { ...(existingInc as PlainObject), version: 1 }
        : { version: 1 };
    result.$inc = inc;
    return result;
  }

  /**
   * Merges the soft-delete condition into a user-supplied filter.
   * When soft delete is enabled, adds `{ deleted_at: null }` so that
   * soft-deleted documents are invisible to all read operations.
   */
  private _mergeFilter(filter: ZeroshipDbFilter): ZeroshipDbFilter {
    if (!this._softDelete) return filter;
    const softFilter: ZeroshipDbFilter = { [this._toColumn("deletedAt")]: null };
    const hasKeys = Object.keys(filter).length > 0;
    return hasKeys ? { $and: [filter, softFilter] } as ZeroshipDbFilter : softFilter;
  }

  /**
   * Inserts a single row after validating it against the schema.
   * Returns the persisted row with `id`, `createdAt`, and `updatedAt` set.
   */
  async insert(row: RowInput<S>): Promise<Result<Row<S>>> {
    return this._run(async () => {
      const validated = validateDoc(row as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const result = await this._nativeCollection().insert(outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>);
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Inserts multiple rows after validating each against the schema.
   * Returns the persisted rows with field names mapped to the user-facing shape.
   */
  async insertMany(rows: RowInput<S>[]): Promise<Result<Row<S>[]>> {
    if (rows.length === 0) return ok([] as Row<S>[]);
    return this._run(async () => {
      const validated = (rows as PlainObject[]).map((r) => validateDoc(r, this._schema));
      const outbound = validated.map((r) => mapDocOutbound(r, this._toColumn));
      const results = await this._nativeCollection().insertMany(outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>[]);
      return (results ?? []).map((r) => mapResultDoc(r as PlainObject, this._toField)) as Row<S>[];
    });
  }

  /**
   * Fetch a single row. The first argument is either an `id` (a bare
   * `number` or `Id<N>` — branded ids stay narrowed) or a full filter
   * object. When the filter matches multiple rows, `opts.orderBy`
   * decides which one is returned; without an orderBy the choice is
   * undefined. Returns `null` if no row matches.
   *
   * `opts.select` projects to a subset of columns; when the array is
   * typed (`["email", "id"] as const` or a `K[]` literal) the return
   * type narrows to `Pick<Row<S>, K> | null` so projected calls don't
   * have to widen back to `Row<S>`.
   */
  async get<K extends string & keyof Row<S>>(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { select: K[]; orderBy?: Record<string, 1 | -1> },
  ): Promise<Result<Pick<Row<S>, K> | null>>;
  async get<W extends WithSpec>(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { with: W; orderBy?: Record<string, 1 | -1> },
  ): Promise<Result<(Row<S> & WithRelations<S, W, AllSchemas>) | null>>;
  async get(
    idOrFilter: number | Id<N> | Filter<S>,
    opts?: { orderBy?: Record<string, 1 | -1> },
  ): Promise<Result<Row<S> | null>>;
  async get(
    idOrFilter: number | Id<N> | Filter<S>,
    opts: { select?: (string & keyof Row<S>)[]; orderBy?: Record<string, 1 | -1>; with?: WithSpec } = {},
  ): Promise<Result<Row<S> | null>> {
    trackCollectionAccess(this._name);
    // DataLoader path: a bare numeric id with no projection / ordering /
    // relation-loading and no active tx. Coalesces concurrent `get(id)`
    // calls in one microtask into a single `WHERE id IN (...)` fetch.
    //
    // Snapshot `_txDepth` BEFORE any await so the loader can detect a
    // tx opening between this call and the next-microtask flush — the
    // loader rejects entries whose snapshot was 0 but find current
    // depth > 0 at flush time. See `loader.ts`.
    const txDepthAtCall = this._txDepth;
    if (
      typeof idOrFilter === "number" &&
      opts.select === undefined &&
      opts.orderBy === undefined &&
      opts.with === undefined &&
      txDepthAtCall === 0
    ) {
      return this._run(() => this._loadById(idOrFilter, txDepthAtCall));
    }
    const filter = (typeof idOrFilter === "number"
      ? ({ id: idOrFilter } as Filter<S>)
      : idOrFilter);
    if (typeof idOrFilter !== "number") {
      _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    }
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const nativeOpts: ZeroshipDbFindOpts = {};
      if (opts.select !== undefined) {
        nativeOpts.select = opts.select.map((f) => this._toColumn(f));
      }
      if (opts.orderBy !== undefined) {
        const mappedOrder: Record<string, 1 | -1> = {};
        for (const [k, v] of Object.entries(opts.orderBy)) {
          mappedOrder[this._toColumn(k)] = v as 1 | -1;
        }
        nativeOpts.orderBy = mappedOrder;
      }
      const result = await this._nativeCollection().findOne(mapped, nativeOpts);
      if (result === null) return null;
      const row = mapResultDoc(result as PlainObject, this._toField);
      if (opts.with !== undefined) {
        await this._loadRelations([row], opts.with);
      }
      return row as Row<S>;
    });
  }

  /** Lazily build the per-collection IdLoader and route the request
   *  through it. The flush callback fires `find({id: {$in: ids}})` via
   *  the native Collection wrapper — this picks up `TX_CONN` routing
   *  in Rust for free. Entries whose enqueue-time snapshot was 0 but
   *  encounter `_txDepth > 0` at flush time are rejected by the loader
   *  (see `loader.ts`) so a non-tx batched read never leaks into a tx
   *  opened mid-batch. */
  private async _loadById(id: number, txDepthAtCall: number): Promise<Row<S> | null> {
    await this.ensureReady();
    if (this._idLoader === null) {
      this._idLoader = new IdLoader<Row<S>>(
        async (ids) => {
          const filter: ZeroshipDbFilter = this._mergeFilter(
            mapFilterOutbound(
              { id: { $in: ids } } as unknown as ZeroshipDbFilter,
              this._toColumn,
            ),
          );
          const rows = (await this._nativeCollection().find(filter, {})) ?? [];
          const map = new Map<number, Row<S>>();
          for (const r of rows) {
            const mapped = mapResultDoc(r as PlainObject, this._toField) as Row<S>;
            map.set(mapped.id, mapped);
          }
          return map;
        },
        () => this._txDepth,
      );
    }
    return this._idLoader.load(id, txDepthAtCall);
  }

  /**
   * Returns true if at least one document matches `filter`.
   *
   * Implemented as `find(filter).limit(1)` so Postgres can short-circuit
   * on an index scan once a single row matches (vs. a full `COUNT(*)`
   * scan). Cost is bounded by the cost of producing one matching row.
   */
  async exists(filter: Filter<S> = {} as Filter<S>): Promise<Result<boolean>> {
    trackCollectionAccess(this._name);
    // Synchronous throws from `this.find(filter)` (e.g. R4 IMPORTANT-1
    // null-filter rejection inside `mapFilterOutbound`) must surface as
    // `Result.error`, not an uncaught throw — `exists` callers expect
    // the same Result-envelope contract as every other Collection
    // read method.
    try {
      const { data, error } = await this.find(filter).limit(1);
      if (error) return err(error);
      return ok((data?.length ?? 0) > 0);
    } catch (e) {
      return err(toResultError(e));
    }
  }

  /**
   * Returns a lazy Query that can be chained with `.sort()`, `.limit()`, `.skip()`,
   * `.select()`, and `.with()` before being awaited.
   *
   * Pass `opts.with` to eager-load referenced rows via the per-collection
   * DataLoader pattern — one batched `find({id: {$in: [...]}})` per relation,
   * not per parent row. Equivalent to `.with(opts.with)` on the returned Query.
   */
  find<W extends WithSpec>(
    filter: Filter<S>,
    opts: { with: W },
  ): Query<S, Row<S> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  find(filter?: Filter<S>): Query<S, Row<S>, AllSchemas>;
  find(filter: Filter<S> = {} as Filter<S>, opts?: { with?: WithSpec }): Query<S, Row<S>, AllSchemas> {
    trackCollectionAccess(this._name);
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
    const q = new Query<S, Row<S>, AllSchemas>(
      this._name,
      mapped,
      async (_col, f, fopts) => {
        await this.ensureReady();
        return this._nativeCollection().find(f, fopts);
      },
      this._toField,
      this._toColumn,
      (rows, spec) => this._loadRelations(rows, spec),
    );
    if (opts?.with !== undefined) q.with(opts.with);
    return q;
  }

  /**
   * Returns the row matching `filter`, or inserts `create` and returns
   * the new row when no match exists. The single SQL statement is
   * `INSERT ... ON CONFLICT DO UPDATE` with a no-op SET; the second
   * return value flags whether the row was freshly created (`true`) or
   * already existed (`false`).
   *
   * `opts.conflictFields` defaults to the unique single-key filter
   * column when the filter has exactly one key and that column carries
   * `.unique()` in the schema. Filters with multiple keys (or a key that
   * isn't unique) require explicit `opts.conflictFields`.
   */
  async findOrCreate(
    filter: Filter<S>,
    create: RowInput<S>,
    opts?: { conflictFields?: (string & keyof Row<S>)[] },
  ): Promise<Result<{ row: Row<S>; created: boolean }>> {
    return this._run(async () => {
      const filterKeys = Object.keys(filter as PlainObject).filter((k) => !k.startsWith("$"));
      let conflictFields = opts?.conflictFields;
      if (conflictFields === undefined || conflictFields.length === 0) {
        if (filterKeys.length !== 1) {
          throw Object.assign(
            new TypeError(
              `findOrCreate: opts.conflictFields is required when filter has ${filterKeys.length} keys (only single-key unique filters auto-infer)`,
            ),
            { code: "find_or_create_needs_conflict_fields" as const },
          );
        }
        const k = filterKeys[0];
        const def = this._schema[k];
        if (!def || def.unique !== true) {
          throw Object.assign(
            new TypeError(
              `findOrCreate: filter key "${k}" is not declared .unique() — pass opts.conflictFields explicitly`,
            ),
            { code: "find_or_create_key_not_unique" as const },
          );
        }
        conflictFields = [k as string & keyof Row<S>];
      }
      // The persisted document is the filter merged into the create
      // payload — filter values are the identity of the row we want, so
      // they always win over `create` for the conflict columns.
      const merged: PlainObject = { ...(create as PlainObject), ...(filter as PlainObject) };
      const validated = validateDoc(merged, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const conflictCols = conflictFields.map((f) => this._toColumn(f as string));
      const colAny = this._nativeCollection() as unknown as {
        findOrCreate(
          doc: Record<string, ZeroshipScalar | ZeroshipScalar[]>,
          opts: { conflictFields: string[] },
        ): Promise<{ row: Record<string, unknown>; created: boolean }>;
      };
      const raw = await colAny.findOrCreate(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
        { conflictFields: conflictCols },
      );
      const row = mapResultDoc(raw.row as PlainObject, this._toField) as Row<S>;
      return { row, created: raw.created === true };
    });
  }

  /**
   * Inserts a row or updates it if a conflict occurs on the specified fields.
   * Returns the persisted row (either newly inserted or updated).
   */
  async upsert(
    row: RowInput<S>,
    options: { conflictFields: (string & keyof Row<S>)[] }
  ): Promise<Result<Row<S>>> {
    return this._run(async () => {
      const validated = validateDoc(row as PlainObject, this._schema);
      const outbound = mapDocOutbound(validated, this._toColumn);
      const conflictCols = options.conflictFields.map((f) => this._toColumn(f));
      const result = await this._nativeCollection().upsert(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
        { conflictFields: conflictCols },
      );
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Updates the first document matching `idOrFilter` and returns the
   * updated document (or `null` if nothing matched). When the first
   * argument is a number it is treated as `{ id: <n> }`; otherwise a
   * full filter is accepted (e.g. compound filters for optimistic
   * concurrency: `{ id, version: 3 }`).
   *
   * `patch` accepts either MongoDB-style operators
   * (`{ $set: {...}, $inc: { n: 1 } }`) or a bare field map (treated as
   * `$set`). The fields are validated against the schema; array push
   * operations are validated against the declared item type.
   * Returns the updated row (or `null` if no row matched).
   */
  async update(
    idOrFilter: number | Filter<S>,
    patch: UpdateExpression<S>
  ): Promise<Result<Row<S> | null>> {
    return this._run(async () => {
      const filter = (typeof idOrFilter === "number"
        ? ({ id: idOrFilter } as Filter<S>)
        : idOrFilter);
      const updateObj = patch as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      // D4 — extract `version: N` from the filter when versioning is on
      // and use it as a CAS guard. The patch is augmented with $inc:1
      // on `version` so the increment happens atomically with the SET.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const result = await this._nativeCollection().updateOne(mappedFilter, mappedUpdate);
      if (result === null) {
        if (casVersion !== null) {
          throw new OptimisticLockError(casVersion, this._name);
        }
        return null;
      }
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Updates all documents matching `filter` using the given `update`.
   * Validates the fields in `$set` and bare keys against the schema.
   * Validates `$push`/`$addToSet` values against the declared array item type.
   * Returns `{ count }` — the number of rows affected. The legacy
   * `{ matchedCount, modifiedCount }` shape always carried identical
   * values (the native layer only reports one count); collapsing to a
   * single field is cleaner.
   */
  async updateMany(
    filter: Filter<S> = {} as Filter<S>,
    update: UpdateExpression<S>
  ): Promise<Result<{ count: number }>> {
    return this._run(async () => {
      const updateObj = update as PlainObject;
      const fields = extractUpdateFields(updateObj);
      checkPartial(fields, this._schema);
      validateArrayPushOps(updateObj, this._schema);
      const casVersion = this._extractCasVersion(filter as PlainObject);
      const augmentedUpdate = this._augmentUpdateWithVersion(updateObj, casVersion);
      const mappedFilter = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const mappedUpdate = mapUpdateOutbound(augmentedUpdate, this._toColumn);
      const n = await this._nativeCollection().updateMany(mappedFilter, mappedUpdate);
      if (n === 0 && casVersion !== null) {
        throw new OptimisticLockError(casVersion, this._name);
      }
      return { count: n };
    });
  }

  /**
   * Deletes the first document matching `idOrFilter` and returns the
   * deleted document (or `null` if nothing matched). When the first
   * argument is a number it is treated as `{ id: <n> }`.
   *
   * When the collection has `softDelete: true`, this sets `deletedAt`
   * instead of removing the row; pass `{ hard: true }` to bypass and
   * permanently remove the row.
   */
  async delete(
    idOrFilter: number | Filter<S>,
    opts: { hard?: boolean } = {},
  ): Promise<Result<Row<S> | null>> {
    return this._run(async () => {
      const filter = (typeof idOrFilter === "number"
        ? ({ id: idOrFilter } as Filter<S>)
        : idOrFilter);
      const hard = opts.hard === true;
      // Both branches honor CAS — versioning + `{ version: N }` in the
      // filter must reject a concurrent writer's lost update, soft or
      // hard. The patch on the soft-delete branch bumps version too.
      const casVersion = this._extractCasVersion(filter as PlainObject);
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const patch = this._augmentUpdateWithVersion(
          { [col]: Date.now() } as PlainObject,
          casVersion,
        );
        const result = await this._nativeCollection().updateOne(mapped, patch as ZeroshipDbUpdate);
        if (result === null) {
          if (casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
          return null;
        }
        return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const result = await this._nativeCollection().deleteOne(mapped);
      if (result === null) {
        if (casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
        return null;
      }
      return mapResultDoc(result as PlainObject, this._toField) as Row<S>;
    });
  }

  /**
   * Deletes all documents matching `filter`. Returns
   * `{ deletedCount: N }`. When the collection has `softDelete: true`,
   * sets `deletedAt` on each row; pass `{ hard: true }` to bypass.
   */
  async deleteMany(
    filter: Filter<S> = {} as Filter<S>,
    opts: { hard?: boolean } = {},
  ): Promise<Result<{ deletedCount: number }>> {
    _maybeWarnUnindexedFilter(this._name, this._schema, filter as PlainObject, this._indexes);
    return this._run(async () => {
      const hard = opts.hard === true;
      const casVersion = this._extractCasVersion(filter as PlainObject);
      if (this._softDelete && !hard) {
        const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
        const col = this._toColumn("deletedAt");
        const patch = this._augmentUpdateWithVersion(
          { [col]: Date.now() } as PlainObject,
          casVersion,
        );
        const n = await this._nativeCollection().updateMany(mapped, patch as ZeroshipDbUpdate);
        if (n === 0 && casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
        return { deletedCount: n };
      }
      const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn);
      const n = await this._nativeCollection().deleteMany(mapped);
      if (n === 0 && casVersion !== null) throw new OptimisticLockError(casVersion, this._name);
      return { deletedCount: n };
    });
  }

  /**
   * Counts documents matching `filter`. Defaults to counting all documents when
   * no filter is provided.
   */
  async count(filter: Filter<S> = {} as Filter<S>): Promise<Result<number>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const n = await this._nativeCollection().count(mapped);
      return typeof n === "number" ? n : 0;
    });
  }

  /**
   * Returns the unique values of `field` across documents matching `filter`.
   * Defaults to all documents when no filter is provided.
   */
  async distinct(field: string & keyof Row<S>, filter: Filter<S> = {} as Filter<S>): Promise<Result<(string | number | boolean | null)[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      if (!this._knownFields.has(field)) {
        throw new ValidationError({ [field]: { path: field, message: `unknown field: ${field}` } });
      }
      const mapped = this._mergeFilter(mapFilterOutbound(filter as ZeroshipDbFilter, this._toColumn));
      const column = this._toColumn(field);
      const result = await this._nativeCollection().distinct(mapped, { field: column });
      return result ?? [];
    });
  }

  /**
   * Runs an aggregation pipeline (MongoDB-style) and returns the mapped results.
   * `$group`, `$match`, and accumulator expressions are translated to the native format.
   */
  async aggregate(pipeline: ZeroshipDbAggregateStage[]): Promise<Result<PlainObject[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      let effectivePipeline: ZeroshipDbAggregateStage[] = pipeline;
      if (this._softDelete) {
        const softFilter: ZeroshipDbFilter = { [this._toColumn("deletedAt")]: null };
        const head = pipeline[0] as { $match?: ZeroshipDbFilter } | undefined;
        if (head && head.$match !== undefined) {
          const existing = head.$match;
          effectivePipeline = [
            { $match: { $and: [existing, softFilter] } as ZeroshipDbFilter },
            ...pipeline.slice(1),
          ];
        } else {
          effectivePipeline = [{ $match: softFilter }, ...pipeline];
        }
      }
      const translated = translateAggregatePipeline(
        effectivePipeline as unknown as PlainObject[],
        this._toColumn,
      ) as ZeroshipDbAggregateStage[];
      const results = await this._nativeCollection().aggregate(translated);
      return (results ?? []).map((d) => mapResultDoc(d as PlainObject, this._toField));
    });
  }

  /**
   * **P4** — vector-nearest-neighbour OR full-text search,
   * discriminated by the presence of `vector` vs. `text` in `args`.
   *
   * ### Vector branch
   *
   * Returns the `k` rows whose `column` vector is closest to `args.vector`
   * by the chosen `metric`. Each returned row carries a synthetic
   * `_distance` field (lower = closer for cosine/L2; higher = closer for
   * `innerProduct` — pgvector negates internally so ORDER BY ASC works
   * uniformly).
   *
   * ```ts
   * const { data } = await db.docs.search({
   *   vector: queryEmbedding,    // number[] — must match the column's
   *                              //   declared dims (1..=16000)
   *   k: 5,                       // 1..=1000 (default 10)
   *   metric: "cosine",          // "cosine" | "l2" | "innerProduct"
   *                              //   (default "cosine")
   *   column: "embedding",       // optional — the vector column name
   *                              //   when more than one is declared
   *   filter: { language: "en" }, // optional WHERE clause
   * });
   * // data: (Row<S> & { _distance: number })[]
   * ```
   *
   * ### Full-text branch
   *
   * Returns rows matching the natural-language query across every
   * column on the collection marked with `.fts()` in the schema.
   * Each row carries a synthetic `_rank` field. PG uses `ts_rank`;
   * SQLite uses bm25 — values are not comparable across backends, but
   * the per-backend ordering is stable.
   *
   * ```ts
   * const { data } = await db.posts.search({
   *   text: "rust async",
   *   limit: 10,                  // 1..=1000 (default 10; alias: `k`)
   *   filter: { lang: "en" },     // optional WHERE clause
   * });
   * // data: (Row<S> & { _rank: number })[]
   * ```
   *
   * ### Errors
   *
   * - `code: "invalid_k"` — `k` (or FTS `limit`) outside 1..=1000.
   * - `ValidationError` — `vector` not an array, `text` not a string,
   *   or neither discriminator present.
   * - `code: "vector_extension_missing"` — PG without `pgvector`.
   *   Run `CREATE EXTENSION vector;` (the `pgvector/pgvector:pg16`
   *   image ships it). See `docs/reference/db.md`.
   * - `code: "vector_dimension_mismatch"` — `args.vector.length` does
   *   not equal the column's declared dims.
   *
   * ### Backend coverage
   *
   * - **PG vector** — routes to pgvector via `VectorIndex::vector_search`.
   *   `ivfflat` index on the column.
   * - **PG text** — routes through `FullTextIndex::fts_search`
   *   (tsvector + GIN, language from `.fts(language)`).
   * - **SQLite vector** — pure-Rust flat scan (`bytemuck::cast_slice` over
   *   `BLOB`). Dev-tier only — degrades past ~50k rows.
   * - **SQLite text** — FTS5 virtual table with the bundled
   *   language-agnostic Unicode tokenizer (the `language` argument is
   *   ignored).
   */
  async search(
    args:
      | {
          vector: number[];
          k?: number;
          metric?: import("./types.js").VectorMetric;
          column?: string;
          filter?: Filter<S>;
        }
      | { text: string; limit?: number; k?: number; filter?: Filter<S> },
  ): Promise<Result<(Row<S> & { _distance?: number; _rank?: number })[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      // Discriminator: presence of `vector` selects the pgvector path;
      // `text` selects the FTS path. The native side does the real
      // dispatch — we keep the SDK layer thin.
      const nativeArgs: {
        vector?: number[];
        text?: string;
        k?: number;
        limit?: number;
        metric?: import("./types.js").VectorMetric;
        column?: string;
        filter?: ZeroshipDbFilter;
      } = {};
      if ("vector" in args && args.vector !== undefined) {
        if (!Array.isArray(args.vector)) {
          throw new ValidationError({
            vector: { path: "vector", message: "search: `vector` must be a number[]" },
          });
        }
        nativeArgs.vector = args.vector;
        if (args.metric !== undefined) nativeArgs.metric = args.metric;
        if (args.column !== undefined) {
          // Map JS field name → DB column name so the native side sees
          // the same identifier the DDL emitted.
          nativeArgs.column = this._toColumn(args.column);
        }
      } else if ("text" in args && args.text !== undefined) {
        if (typeof args.text !== "string") {
          throw new ValidationError({
            text: { path: "text", message: "search: `text` must be a string" },
          });
        }
        nativeArgs.text = args.text;
        // FTS uses `limit` (and tolerates `k` as the legacy alias).
        if ((args as { limit?: number }).limit !== undefined) {
          const lim = (args as { limit?: number }).limit as number;
          _validateK(lim, "limit");
          nativeArgs.limit = lim;
        }
      } else {
        throw new ValidationError({
          args: {
            path: "args",
            message: "search: args must include `vector` or `text`",
          },
        });
      }
      if (args.k !== undefined) {
        _validateK(args.k, "k");
        nativeArgs.k = args.k;
      }
      if (args.filter !== undefined) {
        const mapped = this._mergeFilter(
          mapFilterOutbound(args.filter as ZeroshipDbFilter, this._toColumn),
        );
        nativeArgs.filter = mapped;
      } else if (this._softDelete) {
        // Even without an explicit filter, soft-deleted rows must stay
        // hidden — mirror the read-path defaults.
        nativeArgs.filter = this._mergeFilter({});
      }
      const results = await this._nativeCollection().search(nativeArgs);
      // Map column names back to JS field names; `_distance` / `_rank`
      // pass through because they aren't user fields and `_toField` is
      // a pass-through for unknown columns.
      return (results ?? []).map(
        (d) => mapResultDoc(d as PlainObject, this._toField) as Row<S> & {
          _distance?: number;
          _rank?: number;
        },
      );
    });
  }

  /**
   * **P4 PR 3** — spatial within-radius search.
   *
   * ```ts
   * const { data } = await db.places.near({
   *   field: "location",                    // a t.geoPoint() field
   *   point: { lat: 51.5074, lng: -0.1278 }, // query centre (WGS84)
   *   radius: 1000,                         // metres
   *   filter: { category: "cafe" },         // optional WHERE clause
   *   limit: 50,                            // optional, default 100
   * });
   * // Each row carries a synthetic `_distance_m` field (metres).
   * ```
   *
   * Backend coverage:
   * - **PG** — routes to PostGIS via `SpatialIndex::spatial_near`
   *   (`ST_DWithin` + `ST_Distance`). Errors: `postgis_extension_missing`
   *   when the database lacks PostGIS.
   * - **SQLite** — surfaces `spatial_unsupported` until P4 PR 5 lands
   *   the haversine impl.
   */
  async near(args: {
    field: keyof S & string;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  }): Promise<Result<(Row<S> & { _distance_m: number })[]>> {
    trackCollectionAccess(this._name);
    return this._run(async () => {
      if (typeof args.field !== "string" || args.field.length === 0) {
        throw new ValidationError({
          field: { path: "field", message: "near: `field` must be a non-empty string" },
        });
      }
      if (
        args.point === null ||
        typeof args.point !== "object" ||
        typeof args.point.lat !== "number" ||
        typeof args.point.lng !== "number"
      ) {
        throw new ValidationError({
          point: { path: "point", message: "near: `point` must be `{ lat: number, lng: number }`" },
        });
      }
      if (args.point.lat < -90 || args.point.lat > 90) {
        throw new ValidationError({
          "point.lat": { path: "point.lat", message: "near: `point.lat` must be in [-90, 90]" },
        });
      }
      if (args.point.lng < -180 || args.point.lng > 180) {
        throw new ValidationError({
          "point.lng": { path: "point.lng", message: "near: `point.lng` must be in [-180, 180]" },
        });
      }
      if (typeof args.radius !== "number" || !Number.isFinite(args.radius) || args.radius <= 0) {
        throw new ValidationError({
          radius: { path: "radius", message: "near: `radius` must be a positive finite number (metres)" },
        });
      }

      const nativeArgs: {
        field: string;
        point: { lat: number; lng: number };
        radius: number;
        filter?: ZeroshipDbFilter;
        limit?: number;
      } = {
        field: this._toColumn(args.field as string),
        point: { lat: args.point.lat, lng: args.point.lng },
        radius: args.radius,
      };
      if (args.limit !== undefined) nativeArgs.limit = args.limit;
      if (args.filter !== undefined) {
        const mapped = this._mergeFilter(
          mapFilterOutbound(args.filter as ZeroshipDbFilter, this._toColumn),
        );
        nativeArgs.filter = mapped;
      } else if (this._softDelete) {
        nativeArgs.filter = this._mergeFilter({});
      }

      const colAny = this._nativeCollection() as unknown as {
        near: (a: typeof nativeArgs) => Promise<PlainObject[]>;
      };
      const results = await colAny.near(nativeArgs);
      return (results ?? []).map(
        (d) => mapResultDoc(d as PlainObject, this._toField) as Row<S> & {
          _distance_m: number;
        },
      );
    });
  }

}
