/**
 * Lazy query builder for @zeroship/db.
 * A Query is a thenable that collects sort/limit/skip/select options and
 * executes the native find call only when awaited or .then() is called.
 */
import { mapResultDoc } from "./utils.js";
import {
  InvalidOperationError,
  NotFoundError,
  NotUniqueError,
} from "./errors.js";
import { PlainObject, Result, Row, type WithSpec, type WithRelations, ok, err } from "./types.js";

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
  lastValue: unknown;
  lastId: string;
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
  return btoa(JSON.stringify(state));
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
    decoded = atob(cursor);
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
    typeof (parsed as CursorState).lastId !== "string" ||
    (parsed as CursorState).lastId.length === 0
  ) {
    throw invalid();
  }
  return parsed as CursorState;
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
 * `AllSchemas` is the parent db's full schema map (threaded in by
 * `installSchema` via `Collection<S, N, AllSchemas>`). It lets `.with({ fk:
 * true })` resolve the joined field's type to the target collection's
 * `Row<...>` instead of the safe-default `PlainObject`. Direct `new
 * Query(...)` callers inherit the safe default, so the v1 behaviour is
 * unchanged for tests that build a Query without a parent db.
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
  private _loadRelations: ((rows: PlainObject[], spec: WithSpec) => Promise<void>) | null;

  private _sort: Record<string, number> | undefined;
  private _limit: number | undefined;
  private _skip: number | undefined;
  private _select: string[] | undefined;
  private _afterId: string | undefined;
  private _with: WithSpec | undefined;

  /** @internal */
  constructor(
    collection: string,
    filter: ZeroshipDbFilter,
    native: NativeFn,
    toField?: (s: string) => string,
    toColumn?: (s: string) => string,
    loadRelations?: (rows: PlainObject[], spec: WithSpec) => Promise<void>,
  ) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
    this._toField = toField ?? (s => s);
    this._toColumn = toColumn ?? (s => s);
    this._loadRelations = loadRelations ?? null;
  }

  /**
   * Sets the sort order.
   * Object: `{ field: 1 }` for ASC, `{ field: -1 }` for DESC.
   * String: `"field"` for ASC, `"-field"` for DESC. Multiple: `"-created_at name"`.
   */
  sort(s: Record<string, number> | string): this {
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
      this._sort = s;
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
  after(id: string): this {
    this._afterId = id;
    return this;
  }

  /**
   * Eager-load referenced rows for each key in `spec`. Each key must be a
   * `t.ref(...)` field on the parent schema; the joined row replaces the
   * FK number at that key (or null when the FK is null / target row is missing).
   *
   * Exactly one batched `find({id: {$in: [...]}})` fires per relation across
   * the entire page — no N+1. Type-level narrowing: `.with({user: true})`
   * returns `Query<S, Row<S> & { user: PlainObject | null }>` so the awaited
   * `data[i].user` typechecks without a cast.
   */
  with<W extends WithSpec>(spec: W): Query<S, P & WithRelations<S, W, AllSchemas>, AllSchemas>;
  with(spec: WithSpec): Query<S, any, AllSchemas>;
  with(spec: WithSpec): Query<S, any, AllSchemas> {
    // Reject early when the Query was constructed without a relation
    // loader (e.g. someone called `new Query(...)` directly outside
    // `Collection.find`). The old behaviour was a silent no-op — the
    // `_with` spec accumulated but never fired, so callers got back the
    // bare FK values they hoped to join and had no clue why. A loud
    // TypeError makes the contract explicit.
    if (this._loadRelations === null) {
      throw Object.assign(
        new TypeError(
          "Query.with() requires the Query to be constructed via Collection.find — direct Query construction is not supported",
        ),
        { code: "QUERY_WITH_NO_LOADER" as const },
      );
    }
    this._with = { ...(this._with ?? {}), ...spec };
    return this as unknown as Query<S, any, AllSchemas>;
  }

  /**
   * Restricts the returned fields.
   * String: `"name email"` (space-separated).
   * Array: `["name", "email"]`.
   * Object: `{ name: 1, email: 1 }` (Mongoose style — keys with truthy values).
   *
   * When called with a typed array of literal field names, the return type narrows
   * to `Query<S, Pick<Row<S>, K>>` so that awaited results only contain those fields.
   */
  select<K extends keyof Row<S> & string>(fields: K[]): Query<S, Pick<Row<S>, K>, AllSchemas>;
  select(s: string | string[] | Record<string, number | boolean>): Query<S, P, AllSchemas>;
  select(s: string | string[] | Record<string, number | boolean>): Query<S, any, AllSchemas> {
    if (Array.isArray(s)) {
      this._select = s;
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

    const orderBy: Record<string, 1 | -1> =
      this._sort !== undefined && Object.keys(this._sort).length > 0
        ? (this._sort as Record<string, 1 | -1>)
        : { id: 1 };

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
    const opts2: ZeroshipDbFindOpts = {
      orderBy: this._mapOrderByToColumns(orderBy),
      limit: numItems + 1,
    };
    if (this._select !== undefined) {
      opts2.select = this._select.map((f) => this._toColumn(f));
    }

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
      const page = kept.map((d) => mapResultDoc(d, this._toField)) as P[];

      // Cursor is computed BEFORE relation loading so the orderBy value
      // captured from the last row is the raw column — not an overwritten
      // joined object. `with` keys are FK fields and would be replaced
      // in place by `_loadRelations`.
      //
      // R7 m4 — when `isDone` is true, return `""` (the sentinel for the
      // initial page) so callers keying on `continueCursor === ""` see
      // terminal state. Carrying the input cursor forward would mislead
      // those callers.
      let continueCursor = "";
      if (!isDone && kept.length > 0) {
        const last = page[page.length - 1] as PlainObject;
        const orderKey = Object.keys(orderBy)[0];
        const lastId = last.id;
        if (typeof lastId !== "string" || lastId.length === 0) {
          return err(
            Object.assign(
              new TypeError(`paginate: row id must be a non-empty string (got ${typeof last.id})`),
              { code: "PAGINATE_INVALID_ID" as const },
            ),
          );
        }
        const lastValue = last[orderKey];
        continueCursor = encodeCursor({ orderBy, lastValue, lastId });
      }

      if (this._with !== undefined && this._loadRelations !== null && page.length > 0) {
        await this._loadRelations(page as unknown as PlainObject[], this._with);
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
    const first = keys[0];
    const dir = orderBy[first];
    const lastIdCol = this._toColumn("id");

    if (first === "id") {
      return {
        [lastIdCol]: (dir === 1 ? { $gt: state.lastId } : { $lt: state.lastId }) as ZeroshipDbFilterValue,
      } as ZeroshipDbFilter;
    }

    const firstCol = this._toColumn(first);
    const lastValue = state.lastValue as ZeroshipScalar;
    const strictCmp = (dir === 1 ? { $gt: lastValue } : { $lt: lastValue }) as ZeroshipDbFilterValue;
    const idCmp = (dir === 1 ? { $gt: state.lastId } : { $lt: state.lastId }) as ZeroshipDbFilterValue;
    return {
      $or: [
        { [firstCol]: strictCmp } as ZeroshipDbFilter,
        {
          $and: [
            { [firstCol]: lastValue as ZeroshipDbFilterValue } as ZeroshipDbFilter,
            { [lastIdCol]: idCmp } as ZeroshipDbFilter,
          ],
        } as ZeroshipDbFilter,
      ],
    } as ZeroshipDbFilter;
  }

  /**
   * **P9 PR 1** — terminal returning the first matching row, or `null`
   * when the query has no result. Loose semantics: a missing row is a
   * normal outcome, not an error. Mirrors what `Collection.findOne`
   * used to do — drop the old method's behaviour onto the Query
   * builder.
   *
   * Implementation: applies `LIMIT 1` over the current query state and
   * unwraps the single-row array. The orderBy / select / with / cursor
   * settings carry through unchanged.
   */
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

  /**
   * **P9 PR 1** — strict terminal: exactly one matching row required.
   * Returns `err(NotFoundError)` on 0 matches and `err(NotUniqueError)`
   * on >1 matches. Use this for unique-constraint enforced lookups
   * (e.g. `find({ email }).unique()` against a `.unique()` column)
   * where ambiguity is a contract violation, not a normal outcome.
   *
   * Implementation: `LIMIT 2` so we can detect "more than one" without
   * dragging the whole table; if exactly one row materialises, resolve
   * with it.
   */
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

  /**
   * **P9 PR 1** — terminal returning the last matching row in the
   * current sort, or `null` when there are no matches. Implemented by
   * reversing the configured `.sort(...)` and taking the first row;
   * the original sort is restored before returning.
   *
   * Throws `InvalidOperationError("LAST_REQUIRES_SORT")` (as
   * `err(...)`) if no sort was set on the query — "last" without an
   * ordering would return arbitrary rows from the storage layer.
   */
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

    // Merge cursor condition into filter
    let filter: ZeroshipDbFilter = this._filter;
    if (this._afterId !== undefined) {
      const cursorCondition: ZeroshipDbFilter = { id: { $gt: this._afterId } };
      const hasKeys = Object.keys(filter).length > 0;
      filter = hasKeys
        ? { $and: [filter, cursorCondition] } as ZeroshipDbFilter
        : cursorCondition;
    }

    try {
      const rows = await this._native(this._collection, filter, opts);
      const list: PlainObject[] = Array.isArray(rows) ? rows : [];
      const mapped = list.map(d => mapResultDoc(d, this._toField)) as P[];
      if (this._with !== undefined && this._loadRelations !== null && mapped.length > 0) {
        await this._loadRelations(mapped as unknown as PlainObject[], this._with);
      }
      return ok(mapped);
    } catch (e: unknown) {
      // Rethrow the original Error so any structured `.code` set by the
      // native layer survives. The previous wrapper recreated an Error
      // from only the message string, dropping `.code` along the way.
      return err(e instanceof Error ? e : new Error(String(e)));
    }
  }
}
