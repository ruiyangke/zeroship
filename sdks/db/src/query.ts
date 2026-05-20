/**
 * Lazy query builder for @zeroship/db.
 * A Query is a thenable that collects sort/limit/skip/select options and
 * executes the native find call only when awaited or .then() is called.
 */
import { mapResultDoc } from "./utils.js";
import { PlainObject, Result, Row, ok, err } from "./types.js";

type NativeFn = (
  collection: string,
  filter: ZeroshipDbFilter,
  opts: ZeroshipDbFindOpts
) => Promise<Record<string, unknown>[]>;

/**
 * Chainable query object returned by `Collection.find()`.
 * The generic parameter `S` is the raw schema shape; `P` is the projected document shape.
 * When `.select()` is called with typed field names, `P` narrows to `Pick<Row<S>, K>`.
 * Collects query options lazily and executes via the native layer when awaited.
 */
export class Query<S = PlainObject, P = Row<S>> {
  private _collection: string;
  private _filter: ZeroshipDbFilter;
  private _toField: (s: string) => string;
  private _toColumn: (s: string) => string;
  private _native: NativeFn;

  private _sort: Record<string, number> | undefined;
  private _limit: number | undefined;
  private _skip: number | undefined;
  private _select: string[] | undefined;
  private _afterId: number | undefined;

  /** @internal */
  constructor(
    collection: string,
    filter: ZeroshipDbFilter,
    native: NativeFn,
    toField?: (s: string) => string,
    toColumn?: (s: string) => string,
  ) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
    this._toField = toField ?? (s => s);
    this._toColumn = toColumn ?? (s => s);
  }

  /**
   * Sets the sort order.
   * Object: `{ field: 1 }` for ASC, `{ field: -1 }` for DESC.
   * String: `"field"` for ASC, `"-field"` for DESC. Multiple: `"-createdAt name"`.
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
  after(id: number): this {
    this._afterId = id;
    return this;
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
  select<K extends keyof Row<S> & string>(fields: K[]): Query<S, Pick<Row<S>, K>>;
  select(s: string | string[] | Record<string, number | boolean>): Query<S, P>;
  select(s: string | string[] | Record<string, number | boolean>): Query<S, any> {
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
        throw new Error("exclusion projections (e.g. { field: 0 }) are not supported; use inclusion style: { field: 1 }");
      }
      this._select = entries
        .filter(([, v]) => v)
        .map(([k]) => k);
    }
    return this as unknown as Query<S, any>;
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
      return ok(list.map(d => mapResultDoc(d, this._toField)) as P[]);
    } catch (e: unknown) {
      return err(new Error(`find query failed: ${e instanceof Error ? e.message : String(e)}`, { cause: e }));
    }
  }
}
