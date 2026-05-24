/**
 * Utilities for @zeroship/db.
 *
 * All field↔column mapping is driven by the NamingStrategy passed from Collection.
 * Contains aggregate pipeline translation (MongoDB → native format).
 */
import { PlainObject } from "./types.js";

// ---------------------------------------------------------------------------
// Document mapping (native → user)
// ---------------------------------------------------------------------------

/** @internal Convert column names to JS field names. Returns a new object;
 *  the native side hands back real JS objects we don't own (see commit
 *  9393287), so mutating in place would leak rename side-effects to any
 *  other reference the runtime keeps.
 *
 *  **P9 PR 2** — the `__zsmask__`-sentinel rehydration that used to live
 *  here is gone. Masked columns are now rehydrated Rust-side at
 *  `JSON.parse` time (`ResolveValue::JsonWithRehydration` →
 *  `masked_value::rehydrate_masked_values`): the native row already
 *  carries real `MaskedValue` v8_class instances by the time it reaches
 *  the SDK, so `mapResultDoc` only needs to rename keys. Encrypted
 *  columns are likewise decrypted Rust-side (`apply_encryption_on_read`)
 *  before the row crosses the boundary, so there is no SDK-side
 *  ciphertext arm either.
 */
export function mapResultDoc(doc: PlainObject, toField: (s: string) => string): PlainObject {
  const out: PlainObject = {};
  for (const key of Object.keys(doc)) {
    out[toField(key)] = doc[key];
  }
  return out;
}

/** @internal Convert JS field names to column names for native insert. */
export function mapDocOutbound(doc: PlainObject, toColumn: (s: string) => string): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(doc)) {
    result[toColumn(key)] = val;
  }
  return result;
}

// ---------------------------------------------------------------------------
// Filter mapping (user → native)
// ---------------------------------------------------------------------------

/** @internal Convert JS field names in filter to column names. Recurses into $and/$or/$not. */
export function mapFilterOutbound(filter: ZeroshipDbFilter, toColumn: (s: string) => string, depth = 0): ZeroshipDbFilter {
  if (depth > 20) {
    throw Object.assign(
      new Error("filter nesting too deep (max 20 levels)"),
      { code: "filter_nesting_too_deep" as const },
    );
  }
  // R4 IMPORTANT-1 — null/non-object filter rejection at the boundary.
  // `for (const key in null)` is a zero-iteration no-op (does NOT throw),
  // so a `null` filter used to slip through `needsMap === false`, return
  // verbatim, and reach `_nativeCollection().deleteMany(null)` — where
  // the native side's behaviour on `null` defaults to "matches every
  // row." This is a destructive-by-accident path that the TS types reject
  // but a JSON-RPC caller or an `as any` escape can trip. Reject hard
  // here so every mutating method that funnels through this helper
  // (`deleteMany`, `updateMany`, `delete`, `update`, `find`, ...) gets
  // the guard for free. Inside `_run` the throw becomes `Result.error`
  // with `code = "invalid_filter"`; outside (`find`, which returns a
  // Query synchronously) it propagates to the caller — consistent with
  // every other synchronous schema-violation throw in `Collection`.
  if (filter === null || typeof filter !== "object" || Array.isArray(filter)) {
    throw Object.assign(
      new TypeError(
        `@zeroship/db: filter must be a plain object — got ${
          filter === null ? "null" : Array.isArray(filter) ? "array" : typeof filter
        }. ` +
        `An accidental null filter on deleteMany/updateMany would match every row; ` +
        `pass {} explicitly if you intend to operate on all rows.`,
      ),
      { code: "invalid_filter" as const },
    );
  }
  // Fast path: if no key needs remapping, return the original reference
  let needsMap = false;
  for (const key in filter) {
    if (toColumn(key) !== key || key === "$and" || key === "$or" || key === "$not") {
      needsMap = true;
      break;
    }
  }
  if (!needsMap) return filter;
  const result: ZeroshipDbFilter = {};
  for (const [key, val] of Object.entries(filter)) {
    if (key === "$and" || key === "$or") result[key] = (val as ZeroshipDbFilter[]).map(f => mapFilterOutbound(f, toColumn, depth + 1));
    else if (key === "$not") result[key] = mapFilterOutbound(val as ZeroshipDbFilter, toColumn, depth + 1);
    else result[toColumn(key)] = val;
  }
  return result;
}

// ---------------------------------------------------------------------------
// Update mapping (user → native)
// ---------------------------------------------------------------------------

/** @internal Translate Mongoose top-level operators to per-field native format. */
export function mapUpdateOutbound(update: PlainObject, toColumn: (s: string) => string): ZeroshipDbUpdate {
  const result: ZeroshipDbUpdate = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[toColumn(field)] = fieldVal as ZeroshipDbUpdateValue;
      }
    } else if (key.startsWith("$") && typeof val === "object" && val !== null) {
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[toColumn(field)] = { [key]: fieldVal } as ZeroshipDbUpdateValue;
      }
    } else {
      result[toColumn(key)] = val as ZeroshipDbUpdateValue;
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// Aggregate pipeline translation (MongoDB syntax → native format)
// ---------------------------------------------------------------------------

/** @internal Strip $ prefix from field references */
function stripDollar(val: string): string {
  return val.startsWith("$") ? val.slice(1) : val;
}

/** Aggregate expression — object, string ref, or scalar. */
type AggregateExpr = PlainObject | string | number | boolean | null;

/** Accumulator op names recognised by `translateAccumulator`. Used to
 *  detect unknown `$op` names that would silently pass through. */
const KNOWN_ACCUMULATOR_OPS = new Set([
  "$sum", "$avg", "$min", "$max", "$first", "$count",
]);

/**
 * Shapes already warned about — dedupe by sorted accumulator key set so
 * a noisy pipeline doesn't spam the log.
 *
 * Bounded LRU: a `Map<string, true>` whose insertion-order iteration is
 * guaranteed by the JS spec. On overflow we evict the oldest entry —
 * mirrors `_warnedShapes` in `collection.ts` (Gap P / R3 MINOR-12). An
 * AI-generated pipeline that synthesises new accumulator names would
 * otherwise leak one entry per shape forever.
 */
const MAX_WARNED_ACC_SHAPES = 1024;
const _warnedAccShapes: Map<string, true> = new Map();

/** @internal — record a fired warning for `key`. Returns `true` iff this is
 *  the first time we've seen the shape. Evicts the oldest entry once
 *  `MAX_WARNED_ACC_SHAPES` is hit. */
function _noteWarnedAccShape(key: string): boolean {
  if (_warnedAccShapes.has(key)) return false;
  if (_warnedAccShapes.size >= MAX_WARNED_ACC_SHAPES) {
    const oldest = _warnedAccShapes.keys().next().value;
    if (oldest !== undefined) _warnedAccShapes.delete(oldest);
  }
  _warnedAccShapes.set(key, true);
  return true;
}

/** @internal — test-only reset hook. */
export function __zeroshipDbResetAccShapeWarnings(): void {
  _warnedAccShapes.clear();
}

/** @internal — test-only size accessor. */
export function __zeroshipDbWarnedAccShapesSize(): number {
  return _warnedAccShapes.size;
}

/** @internal Translate an accumulator expression */
function translateAccumulator(acc: AggregateExpr): AggregateExpr {
  if (typeof acc !== "object" || acc === null) return acc;
  const obj = acc as PlainObject;

  if ("$sum" in obj && obj.$sum === 1) return { $count: true };
  if ("$sum" in obj && typeof obj.$sum === "string") return { $sum: stripDollar(obj.$sum as string) };
  if ("$avg" in obj && typeof obj.$avg === "string") return { $avg: stripDollar(obj.$avg as string) };
  if ("$min" in obj && typeof obj.$min === "string") return { $min: stripDollar(obj.$min as string) };
  if ("$max" in obj && typeof obj.$max === "string") return { $max: stripDollar(obj.$max as string) };
  if ("$first" in obj && typeof obj.$first === "string") return { $first: stripDollar(obj.$first as string) };

  // Surface unknown `$op`s once per shape — silent pass-through means the
  // operator never reaches Postgres and the user gets a confusingly empty
  // result rather than a hint that the op was unrecognised.
  const opKeys = Object.keys(obj).filter((k) => k.startsWith("$"));
  const unknown = opKeys.filter((k) => !KNOWN_ACCUMULATOR_OPS.has(k));
  if (unknown.length > 0) {
    const shapeKey = unknown.slice().sort().join(",");
    if (_noteWarnedAccShape(shapeKey)) {
      console.warn(
        `[@zeroship/db] aggregate accumulator ${unknown.join(", ")} ` +
        `is not recognised and will be passed through unchanged — ` +
        `it likely will not reach Postgres. Supported: ${Array.from(KNOWN_ACCUMULATOR_OPS).join(", ")}.`,
      );
    }
  }
  return acc;
}

/** @internal Translate _id group key to by */
function translateGroupId(id: AggregateExpr): { by: string | string[] } {
  if (typeof id === "string") return { by: stripDollar(id) };
  if (typeof id === "object" && id !== null) {
    const fields: string[] = [];
    for (const val of Object.values(id as PlainObject)) {
      if (typeof val === "string") fields.push(stripDollar(val));
    }
    return { by: fields };
  }
  return { by: String(id) };
}

/** @internal Translate a single pipeline stage */
function translateStage(stage: PlainObject, toColumn: (s: string) => string): PlainObject {
  if ("$match" in stage) {
    return { $match: mapFilterOutbound(stage.$match as ZeroshipDbFilter, toColumn) };
  }
  if ("$group" in stage) {
    const group = stage.$group as PlainObject;
    const groupKey = (group._id ?? group.id) as AggregateExpr;
    const { _id: _discardId, id: _discardId2, ...rest } = group;
    const { by } = translateGroupId(groupKey);
    const translated: PlainObject = { by };
    for (const [key, val] of Object.entries(rest)) {
      translated[key] = translateAccumulator(val as AggregateExpr);
    }
    return { $group: translated };
  }
  if ("$having" in stage) {
    return { $having: mapFilterOutbound(stage.$having as ZeroshipDbFilter, toColumn) };
  }
  if ("$sort" in stage) {
    const sort = stage.$sort as PlainObject;
    const mapped: PlainObject = {};
    for (const [key, val] of Object.entries(sort)) {
      mapped[toColumn(key)] = val;
    }
    return { $sort: mapped };
  }
  return stage;
}

/**
 * @internal
 * Translate a MongoDB-style aggregate pipeline to native format.
 */
export function translateAggregatePipeline(pipeline: PlainObject[], toColumn: (s: string) => string): PlainObject[] {
  return pipeline.map(stage => translateStage(stage, toColumn));
}
