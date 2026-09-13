import { trackRelations, type ReadResultMapper } from "./collection/read-mapping";
import type { FieldDef } from "./types";
/**
 * Lazy query builder for @zeroship/db.
 * A Query is a thenable that collects sort/limit/skip/select options and
 * executes the native find call only when awaited or .then() is called.
 */
import { mapResultDoc } from "./utils";
import { isIdValue } from "./identity.js";
import {
  InvalidOperationError,
  NotFoundError,
  NotUniqueError,
} from "./errors";
import {
  PlainObject,
  Result,
  Row,
  type Actor,
  type ExactWithSpec,
  type IdValue,
  type RowId,
  type SelectableField,
  type SelectInput,
  type SelectSpec,
  type SortInput,
  type WithRelations,
  type WithSpec,
  ok,
  err,
} from "./types";

type NativeFn = (
  collection: string,
  filter: ZeroshipDbFilter,
  opts: ZeroshipDbFindOpts
) => Promise<Record<string, unknown>[]>;

/**
 * Decoded shape of the opaque `continueCursor` string returned by
 * `Query.paginate()`. The cursor carries the orderBy in JS-field space
 * (matching `Query._sort`) plus the last row's orderBy value and id —
 * enough to seek past the previous page on the next call.
 */
type CursorState = {
  orderBy: Record<string, 1 | -1>;
  /** Values for every ordering key in the last returned row. */
  lastValues: Record<string, unknown>;
  lastId: IdValue;
};

/** Page envelope returned by `Query.paginate()`. Matches Convex's shape so
 *  a future `useZShipPaginatedQuery` hook can adopt it without translation. */
export type PaginationResult<R> = {
  page: R[];
  continueCursor: string;
  isDone: boolean;
};

/** Base64-encode a CursorState using `btoa` so the cursor is a plain
 *  opaque string callers can round-trip through query params / URLs. */
function encodeCursor(state: CursorState): string {
  const json = JSON.stringify({
    orderBy: state.orderBy,
    lastValues: Object.fromEntries(Object.entries(state.lastValues).map(([key, value]) => [key, encodeCursorValue(value)])),
    lastId: encodeCursorValue(state.lastId),
  });
  return btoa(Array.from(new TextEncoder().encode(json), byte => String.fromCharCode(byte)).join(""));
}

function encodeCursorValue(value: unknown): unknown {
  if (typeof value === "bigint") return { bigint: value.toString() };
  // Wrap JSON values so their keys cannot be mistaken for scalar type tags.
  return value !== null && typeof value === "object" ? { json: value } : value;
}

function decodeCursorValue(value: unknown): unknown {
  if (value === null || typeof value !== "object") return value;
  if (Object.keys(value).length === 1) {
    if ("json" in value) return value.json;
    if ("bigint" in value && typeof value.bigint === "string" && /^-?(0|[1-9]\d*)$/.test(value.bigint)) {
      return BigInt(value.bigint);
    }
  }
  throw new TypeError("invalid cursor value");
}

/** Decode and shape-check a base64-JSON cursor string. Throws with
 *  `code: "PAGINATE_INVALID_CURSOR"` on any malformed input — the
 *  paginate caller catches this and returns it as `Result.error`. */
function decodeCursor(cursor: string): CursorState {
  const invalid = (): Error =>
    Object.assign(new Error("paginate: invalid cursor"), {
      code: "PAGINATE_INVALID_CURSOR" as const,
    });
  let decoded: string;
  try {
    decoded = new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(atob(cursor), char => char.charCodeAt(0)));
  } catch {
    throw invalid();
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(decoded);
  } catch {
    throw invalid();
  }
  if (
    parsed === null ||
    typeof parsed !== "object" ||
    Array.isArray(parsed) ||
    typeof (parsed as CursorState).orderBy !== "object" ||
    (parsed as CursorState).orderBy === null ||
    Array.isArray((parsed as CursorState).orderBy) ||
    typeof (parsed as CursorState).lastValues !== "object" ||
    (parsed as CursorState).lastValues === null ||
    Array.isArray((parsed as CursorState).lastValues)
  ) {
    throw invalid();
  }
  const state = parsed as CursorState;
  try {
    const lastId = decodeCursorValue(state.lastId);
    if (!isIdValue(lastId) || lastId === "") throw invalid();
    const lastValues = Object.fromEntries(Object.entries(state.lastValues).map(([key, value]) => [key, decodeCursorValue(value)]));
    for (const [key, direction] of Object.entries(state.orderBy)) {
      if ((direction !== 1 && direction !== -1) || !Object.hasOwn(lastValues, key)) throw invalid();
    }
    return { orderBy: state.orderBy, lastValues, lastId };
  } catch {
    throw invalid();
  }
}

/** Compare two `{ field: 1 | -1 }` orderBy objects key-set and direction.
 *  Cursors are bound to the orderBy that produced them — different sort
 *  means the seek predicate is meaningless. */
function sameOrderBy(a: Record<string, 1 | -1>, b: Record<string, 1 | -1>): boolean {
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (let i = 0; i < ka.length; i++) {
    if (ka[i] !== kb[i]) return false;
    if (a[ka[i]] !== b[ka[i]]) return false;
  }
  return true;
}

/**
 * Chainable query object returned by `Collection.find()`.
 * The generic parameter `S` is the raw schema shape; `P` is the projected document shape.
 * When `.select()` is called with typed field names, `P` narrows to `Pick<Row<S>, K>`.
 *
 * `AllSchemas` resolves named relation edges to their target row types.
 *
 * Collects query options lazily and executes via the native layer when awaited.
 */
export class Query<
  S = PlainObject,
  P = Row<S>,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  private _collection: string;
  private _filter: ZeroshipDbFilter;
  private _toField: (s: string) => string;
  private _toColumn: (s: string) => string;
  private _native: NativeFn;
  private _mapReadResult: ReadResultMapper;

  private _sort: Record<string, number> | undefined;
  private _limit: number | undefined;
  private _skip: number | undefined;
  private _select: string[] | undefined;
  private _afterId: RowId<S> | undefined;
  private _with: WithSpec<S> | undefined;
  private _unmask: string[] | undefined;
  private _actor: Actor | undefined;
  private _unmaskReason: string | undefined;

  /** @internal */
  constructor(
    collection: string,
    filter: ZeroshipDbFilter,
    native: NativeFn,
    toField?: (s: string) => string,
    toColumn?: (s: string) => string,
    mapReadResult?: ReadResultMapper,
    readHints?: {
      unmask?: string[];
      actor?: Actor;
      unmaskReason?: string;
    },
    private readonly _schema: Record<string, FieldDef> = {},
  ) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
    this._toField = toField ?? (s => s);
    this._toColumn = toColumn ?? (s => s);
    this._mapReadResult = mapReadResult ?? (row => mapResultDoc(row, this._toField));
    this._unmask = readHints?.unmask;
    this._actor = readHints?.actor;
    this._unmaskReason = readHints?.unmaskReason;
  }

  /**
   * Sets the sort order.
   * Objects can order by several fields: `{ score: -1, title: 1 }`.
   * Strings order by one field: `"title"` or `"-score"`.
   */
  sort(s: SortInput<S>): this {
    if (typeof s === "string") {
      const obj: Record<string, number> = {};
      for (const part of s.split(/\s+/).filter(Boolean)) {
        if (part.startsWith("-")) {
          obj[part.slice(1)] = -1;
        } else {
          obj[part] = 1;
        }
      }
      this._sort = obj;
    } else {
      this._sort = s as Record<string, number>;
    }
    return this;
  }

  /** Limits the number of documents returned. */
  limit(n: number): this {
    this._limit = n;
    return this;
  }

  /** Skips the first `n` documents (for pagination). */
  skip(n: number): this {
    this._skip = n;
    return this;
  }

  /**
   * Cursor-based pagination: returns documents with `id > afterId`.
   * Merges an `{ id: { $gt: afterId } }` condition into the filter at execution time.
   */
  after(id: RowId<S>): this {
    this._afterId = id;
    return this;
  }

  /** Load named schema edges while preserving their scalar foreign-key fields. */
  with<const W extends WithSpec<S>>(
    spec: ExactWithSpec<S, W>,
  ): Query<S, Omit<P, keyof W> & WithRelations<S, W, AllSchemas>, AllSchemas>;
  with(spec: WithSpec<S>): Query<S, any, AllSchemas> {
    trackRelations(this._schema, spec);
    this._with = { ...(this._with ?? {}), ...spec } as WithSpec<S>;
    return this as unknown as Query<S, any, AllSchemas>;
  }

  /**
   * Restricts the returned fields.
   * String: `"name"`.
   * Array: `["name", "email"]`.
   * Object: `{ name: 1, email: 1 }` (Mongoose style — keys with truthy values).
   * Untyped direct queries also accept a space-separated string.
   *
   * When called with a typed array of literal field names, the return type narrows
   * to `Query<S, Pick<Row<S>, K>>` so that awaited results only contain those fields.
   */
  select<K extends SelectableField<S>>(field: K): Query<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<K extends SelectableField<S>>(fields: readonly K[]): Query<S, Pick<Row<S>, K> & Omit<P, keyof Row<S>>, AllSchemas>;
  select<const Selection extends SelectSpec<S>>(
    fields: Selection,
  ): Query<S, Pick<Row<S>, keyof Selection & keyof Row<S>> & Omit<P, keyof Row<S>>, AllSchemas>;
  select(s: SelectInput<S>): Query<S, any, AllSchemas> {
    if (Array.isArray(s)) {
      this._select = [...s];
    } else if (typeof s === "string") {
      this._select = s.split(" ").filter((f) => f.length > 0);
    } else {
      // Object style: { name: 1, email: 1 } → ["name", "email"]
      // Exclusion style { password: 0 } is not supported — reject it
      const entries = Object.entries(s);
      const allFalsy = entries.length > 0 && entries.every(([, v]) => !v);
      if (allFalsy) {
        throw Object.assign(
          new Error("exclusion projections (e.g. { field: 0 }) are not supported; use inclusion style: { field: 1 }"),
          { code: "QUERY_EXCLUSION_NOT_SUPPORTED" as const },
        );
      }
      this._select = entries
        .filter(([, v]) => v)
        .map(([k]) => k);
    }
    return this as unknown as Query<S, any, AllSchemas>;
  }

  /**
   * Cursor-paginate the query. Pass `cursor: null` (or omit) for the first
   * page; pass back `continueCursor` from the previous result to advance.
   * `isDone` is `true` once the underlying store returns fewer than
   * `numItems + 1` rows — the page can be rendered as the final page.
   *
   * The seek predicate is built from the Query's current `.sort(...)`
   * (defaulting to `{ id: 1 }`) so paginate gracefully degenerates to
   * id-only ordering. Cursors are opaque base64-JSON and bound to the
   * orderBy they were produced under — passing a cursor from a query
   * with a different sort rejects with `paginate: cursor orderBy mismatch`.
   */
  async paginate(opts: {
    cursor?: string | null;
    numItems: number;
  }): Promise<Result<PaginationResult<P>>> {
    const { cursor, numItems } = opts;
    if (!Number.isInteger(numItems) || numItems <= 0) {
      return err(
        Object.assign(
          new TypeError("paginate: numItems must be a positive integer"),
          { code: "PAGINATE_INVALID_NUM_ITEMS" as const },
        ),
      );
    }

    const key = "id";

    const orderBy: Record<string, 1 | -1> =
      this._sort !== undefined && Object.keys(this._sort).length > 0
        ? (this._sort as Record<string, 1 | -1>)
        : { [key]: 1 };

    let cursorState: CursorState | null = null;
    if (cursor !== null && cursor !== undefined) {
      try {
        cursorState = decodeCursor(cursor);
      } catch (e) {
        return err(e instanceof Error ? e : new Error(String(e)));
      }
      if (!sameOrderBy(cursorState.orderBy, orderBy)) {
        return err(
          Object.assign(new Error("paginate: cursor orderBy mismatch"), {
            code: "PAGINATE_ORDERBY_MISMATCH" as const,
          }),
        );
      }
    }

    // Build the page-window query: apply orderBy + limit(numItems+1) so
    // we can detect isDone by whether the +1 row materialised. The cursor
    // predicate is OR-merged into the existing filter at the column layer.
    // The emitted order must be the SAME tuple the seek compares, including
    // the id tiebreak. Without it the sort is only a partial order: ties in
    // the caller's keys are broken by whatever the store happens to return,
    // which the seek then assumes was id order. Appending id here is what
    // makes `_buildSeekFilter`'s final disjunct meaningful rather than
    // aspirational.
    const seekOrder: Record<string, 1 | -1> = { ...orderBy };
    if (!(key in seekOrder)) seekOrder[key] = 1;

    const opts2: ZeroshipDbFindOpts = {
      orderBy: this._mapOrderByToColumns(seekOrder),
      limit: numItems + 1,
    };
    if (this._select !== undefined) {
      opts2.select = this._select.map((f) => this._toColumn(f));
    }
    if (this._unmask !== undefined) {
      opts2.unmask = this._unmask.map((f) => this._toColumn(f));
    }
    if (this._actor !== undefined) {
      opts2.actor = this._actor;
    }
    if (this._unmaskReason !== undefined) {
      opts2.unmaskReason = this._unmaskReason;
    }

    if (this._with !== undefined) opts2.with = this._with as Record<string, true>;

    let filter: ZeroshipDbFilter = this._filter;
    if (cursorState !== null) {
      const seek = this._buildSeekFilter(orderBy, cursorState);
      const hasKeys = Object.keys(filter).length > 0;
      filter = hasKeys
        ? ({ $and: [filter, seek] } as ZeroshipDbFilter)
        : seek;
    }

    try {
      const raw = await this._native(this._collection, filter, opts2);
      const rows: PlainObject[] = Array.isArray(raw) ? raw : [];
      const isDone = rows.length <= numItems;
      const kept = isDone ? rows : rows.slice(0, numItems);
      const page = kept.map((row) => this._mapReadResult(row, this._with)) as P[];

      let continueCursor = "";
      if (!isDone && kept.length > 0) {
        const last = page[page.length - 1] as PlainObject;
        const lastId = last[key];
        if (!isIdValue(lastId) || lastId === "") {
          return err(
            Object.assign(
              new TypeError("paginate: row id must be text or a finite numeric value"),
              { code: "PAGINATE_INVALID_ID" as const },
            ),
          );
        }
        const lastValues: Record<string, unknown> = {};
        for (const field of Object.keys(orderBy)) lastValues[field] = last[field];
        continueCursor = encodeCursor({ orderBy, lastValues, lastId });
      }

      return ok({ page, continueCursor, isDone });
    } catch (e: unknown) {
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }

  /** @internal — translate a JS-space orderBy object to native column space. */
  private _mapOrderByToColumns(orderBy: Record<string, 1 | -1>): Record<string, 1 | -1> {
    const out: Record<string, 1 | -1> = {};
    for (const [k, v] of Object.entries(orderBy)) {
      out[this._toColumn(k)] = v;
    }
    return out;
  }

  /** @internal — build the seek-after predicate for `paginate`. For an
   *  ascending sort on `F` the predicate is `F > lastValue OR (F = lastValue
   *  AND id > lastId)`; descending flips the comparators. When the orderBy
   *  is id-only the compound clause collapses to a single inequality. */
  private _buildSeekFilter(
    orderBy: Record<string, 1 | -1>,
    state: CursorState,
  ): ZeroshipDbFilter {
    const keys = Object.keys(orderBy);
    const lastIdCol = this._toColumn("id");

    // Lexicographic seek over (k1, .., kn, id) - the SAME tuple the emitted
    // ORDER BY uses, which is what makes it sound. For each key i, one
    // disjunct: "keys 1..i-1 equal, key i strictly past its last value",
    // then a final disjunct with every key equal and id past its last value.
    //
    // The previous form compared only keys[0] and id, so it was sound only
    // when the order really was (keys[0], id). With any second sort key the
    // order was (k1, k2, ..) while the seek asked for (k1, id), and rows
    // sorting after the boundary but carrying a smaller id were skipped -
    // permanently, since no later page ever asks for them again.
    const terms: ZeroshipDbFilter[] = [];
    const eqPrefix: ZeroshipDbFilter[] = [];

    for (const k of keys) {
      const col = this._toColumn(k);
      const v = state.lastValues[k] as ZeroshipScalar;
      const cmp = (orderBy[k] === 1 ? { $gt: v } : { $lt: v }) as ZeroshipDbFilterValue;
      const strict = { [col]: cmp } as ZeroshipDbFilter;
      terms.push(
        eqPrefix.length === 0
          ? strict
          : ({ $and: [...eqPrefix, strict] } as ZeroshipDbFilter),
      );
      eqPrefix.push({ [col]: v as ZeroshipDbFilterValue } as ZeroshipDbFilter);
    }

    // The id tiebreak, unless `id` is already one of the ordering keys - in
    // which case the loop above has already compared it and appending another
    // term would add an unsatisfiable disjunct (id = X AND id > X).
    if (!keys.includes("id")) {
      const idCmp = { $gt: state.lastId } as ZeroshipDbFilterValue;
      terms.push({ $and: [...eqPrefix, { [lastIdCol]: idCmp } as ZeroshipDbFilter] } as ZeroshipDbFilter);
    }

    return (terms.length === 1 ? terms[0] : { $or: terms }) as ZeroshipDbFilter;
  }

  /** Return the first matching row, or `null`. */
  async first(): Promise<Result<P | null>> {
    const prevLimit = this._limit;
    this._limit = 1;
    try {
      const result = await this._exec();
      if (result.error !== null) return err(result.error);
      const list = result.data!;
      return ok(list.length === 0 ? null : list[0]);
    } finally {
      this._limit = prevLimit;
    }
  }

  /** Return one row, failing when none or multiple rows match. */
  async unique(): Promise<Result<P>> {
    const prevLimit = this._limit;
    this._limit = 2;
    try {
      const result = await this._exec();
      if (result.error !== null) return err(result.error);
      const list = result.data!;
      if (list.length === 0) {
        return err(new NotFoundError(this._collection));
      }
      if (list.length > 1) {
        return err(new NotUniqueError(list.length, this._collection));
      }
      return ok(list[0]);
    } finally {
      this._limit = prevLimit;
    }
  }

  /** Return the last row in the configured order, or fail when no order is set. */
  async last(): Promise<Result<P | null>> {
    if (this._sort === undefined || Object.keys(this._sort).length === 0) {
      return err(
        new InvalidOperationError(
          "LAST_REQUIRES_SORT",
          "Query.last() requires a .sort(...) clause — 'last' is meaningless without an ordering",
        ),
      );
    }
    const prevSort = this._sort;
    const prevLimit = this._limit;
    const reversed: Record<string, number> = {};
    for (const [k, v] of Object.entries(prevSort)) {
      reversed[k] = v === 1 ? -1 : 1;
    }
    this._sort = reversed;
    this._limit = 1;
    try {
      const result = await this._exec();
      if (result.error !== null) return err(result.error);
      const list = result.data!;
      return ok(list.length === 0 ? null : list[0]);
    } finally {
      this._sort = prevSort;
      this._limit = prevLimit;
    }
  }

  /**
   * Makes Query thenable so it can be used with `await`.
   * Executes the query and passes results to `resolve`; calls `reject` on error.
   */
  then<TResult1 = Result<P[]>, TResult2 = never>(
    resolve?: ((value: Result<P[]>) => TResult1 | PromiseLike<TResult1>) | null,
    reject?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null
  ): Promise<TResult1 | TResult2> {
    return this._exec().then(resolve, reject);
  }

  /** Executes the query and returns the mapped result documents. */
  async _exec(): Promise<Result<P[]>> {
    const opts: ZeroshipDbFindOpts = {};
    if (this._sort !== undefined) {
      // Map JS field names → DB column names for the native call.
      const mapped: Record<string, 1 | -1> = {};
      for (const [k, v] of Object.entries(this._sort)) {
        mapped[this._toColumn(k)] = v as 1 | -1;
      }
      opts.orderBy = mapped;
    }
    if (this._limit !== undefined) opts.limit = this._limit;
    if (this._skip !== undefined) opts.offset = this._skip;
    if (this._select !== undefined) opts.select = this._select.map(f => this._toColumn(f));
    if (this._unmask !== undefined) opts.unmask = this._unmask.map(f => this._toColumn(f));
    if (this._actor !== undefined) opts.actor = this._actor;
    if (this._unmaskReason !== undefined) opts.unmaskReason = this._unmaskReason;
    if (this._with !== undefined) opts.with = this._with as Record<string, true>;

    // Merge cursor condition into filter
    let filter: ZeroshipDbFilter = this._filter;
    if (this._afterId !== undefined) {
      const cursorCondition: ZeroshipDbFilter = { [this._toColumn("id")]: { $gt: this._afterId } };
      const hasKeys = Object.keys(filter).length > 0;
      filter = hasKeys
        ? { $and: [filter, cursorCondition] } as ZeroshipDbFilter
        : cursorCondition;
    }

    try {
      const rows = await this._native(this._collection, filter, opts);
      const list: PlainObject[] = Array.isArray(rows) ? rows : [];
      const mapped = list.map(row => this._mapReadResult(row, this._with)) as P[];
      return ok(mapped);
    } catch (e: unknown) {
      // Rethrow the original Error so any structured `.code` set by the
      // native layer survives. The previous wrapper recreated an Error
      // from only the message string, dropping `.code` along the way.
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }
}
