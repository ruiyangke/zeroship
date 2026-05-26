import type { NormalizedSchema } from "../schema";
import type { NamedIndexSpec, PlainObject } from "../types";

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
export function _maybeWarnUnindexedFilter(
  collection: string,
  schema: NormalizedSchema,
  filter: PlainObject,
  declaredIndexes: readonly NamedIndexSpec[],
): void {
  const nodeEnv = (
    globalThis as { process?: { env?: { NODE_ENV?: string } } }
  ).process?.env?.NODE_ENV;
  if (nodeEnv === "production") return;
  const opt = (
    globalThis as { __zeroshipDbWarnIndexInTest?: boolean }
  ).__zeroshipDbWarnIndexInTest;
  if (nodeEnv === "test" && opt !== true) return;

  if (filter === null || typeof filter !== "object") return;
  const keys = Object.keys(filter).filter(
    (k) => !k.startsWith("$") && k in schema,
  );
  if (keys.length === 0) return;
  if (keys.length === 1 && keys[0] === "id") return;

  if (_filterCoveredByIndex(keys, schema, declaredIndexes)) return;

  const shapeKey = `${collection}:${[...keys].sort().join(",")}`;
  if (!_noteWarnedShape(shapeKey)) return;

  const declaredNames = declaredIndexes.map((i) => i.name);
  const declaredHint =
    declaredNames.length > 0
      ? `Declared indexes: ${declaredNames.join(", ")}.`
      : "No indexes declared on this collection.";
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
  for (const k of keys) {
    const def = schema[k];
    if (!def || (def.index !== true && def.unique !== true)) return false;
  }
  return true;
}
