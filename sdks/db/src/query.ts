/**
 * Lazy query builder for @appbase/db.
 * A Query is a thenable that collects sort/limit/skip/select options and
 * executes the native find call only when awaited or .then() is called.
 */
import { mapResultDoc } from "./utils.js";
import { PlainObject } from "./types.js";

type NativeFn = (
  collection: string,
  filter: PlainObject,
  opts: PlainObject
) => Promise<string>;

/**
 * Chainable query object returned by `Collection.find()`.
 * Collects query options lazily and executes via the native layer when awaited.
 */
export class Query {
  private _collection: string;
  private _filter: PlainObject;
  private _native: NativeFn;

  private _sort: PlainObject | undefined;
  private _limit: number | undefined;
  private _skip: number | undefined;
  private _select: string[] | undefined;

  /** @internal */
  constructor(
    collection: string,
    filter: PlainObject,
    native: NativeFn
  ) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
  }

  /** Sets the sort order. Pass `{ field: 1 }` for ascending, `{ field: -1 }` for descending. */
  sort(obj: PlainObject): this {
    this._sort = obj;
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
   * Restricts the returned fields.
   * Accepts either a space-separated string (`"name age"`) or an array of field names.
   */
  select(s: string | string[]): this {
    if (Array.isArray(s)) {
      this._select = s;
    } else {
      this._select = s.split(" ").filter((f) => f.length > 0);
    }
    return this;
  }

  /**
   * Makes Query thenable so it can be used with `await`.
   * Executes the query and passes results to `resolve`; calls `reject` on error.
   */
  then(
    resolve: (value: PlainObject[]) => unknown,
    reject?: (reason: unknown) => unknown
  ): Promise<unknown> {
    return this._exec().then(resolve, reject);
  }

  /** Executes the query and returns the mapped result documents. */
  async _exec(): Promise<PlainObject[]> {
    const opts: PlainObject = {};
    if (this._sort !== undefined) opts["sort"] = this._sort;
    if (this._limit !== undefined) opts["limit"] = this._limit;
    if (this._skip !== undefined) opts["skip"] = this._skip;
    if (this._select !== undefined) opts["select"] = this._select;

    try {
      const raw = await this._native(this._collection, this._filter, opts);
      const parsed: unknown = typeof raw === "string" ? JSON.parse(raw) : raw;
      const rows: PlainObject[] = Array.isArray(parsed) ? parsed : [];
      return rows.map(mapResultDoc);
    } catch (e: any) {
      throw new Error(`find query failed: ${e.message ?? e}`, { cause: e });
    }
  }
}
