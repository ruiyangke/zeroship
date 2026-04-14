/**
 * Lazy query builder for @zeroship/db.
 * A Query is a thenable that collects sort/limit/skip/select options and
 * executes the native find call only when awaited or .then() is called.
 */
import { mapResultDoc } from "./utils.js";
import { PlainObject, Result, Document, ok, err } from "./types.js";

type NativeFn = (
  collection: string,
  filter: PlainObject,
  opts: PlainObject
) => Promise<string>;

/**
 * Chainable query object returned by `Collection.find()`.
 * The generic parameter `S` is the raw schema shape; results resolve to `Document<S>[]`.
 * Collects query options lazily and executes via the native layer when awaited.
 */
export class Query<S = PlainObject> {
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

  /**
   * Sets the sort order.
   * Object: `{ field: 1 }` for ASC, `{ field: -1 }` for DESC.
   * String: `"field"` for ASC, `"-field"` for DESC. Multiple: `"-createdAt name"`.
   */
  sort(s: PlainObject | string): this {
    if (typeof s === "string") {
      const obj: PlainObject = {};
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
   * Restricts the returned fields.
   * String: `"name email"` (space-separated).
   * Array: `["name", "email"]`.
   * Object: `{ name: 1, email: 1 }` (Mongoose style — keys with truthy values).
   */
  select(s: string | string[] | Record<string, unknown>): this {
    if (Array.isArray(s)) {
      this._select = s;
    } else if (typeof s === "string") {
      this._select = s.split(" ").filter((f) => f.length > 0);
    } else {
      // Object style: { name: 1, email: 1 } → ["name", "email"]
      this._select = Object.entries(s)
        .filter(([, v]) => v)
        .map(([k]) => k);
    }
    return this;
  }

  /**
   * Makes Query thenable so it can be used with `await`.
   * Executes the query and passes results to `resolve`; calls `reject` on error.
   */
  then(
    resolve?: (value: Result<Document<S>[]>) => unknown,
    reject?: (reason: unknown) => unknown
  ): Promise<unknown> {
    return this._exec().then(resolve as any, reject);
  }

  /** Executes the query and returns the mapped result documents. */
  async _exec(): Promise<Result<Document<S>[]>> {
    const opts: PlainObject = {};
    if (this._sort !== undefined) opts["sort"] = this._sort;
    if (this._limit !== undefined) opts["limit"] = this._limit;
    if (this._skip !== undefined) opts["skip"] = this._skip;
    if (this._select !== undefined) opts["select"] = this._select;

    try {
      const raw = await this._native(this._collection, this._filter, opts);
      const parsed: unknown = typeof raw === "string" ? JSON.parse(raw) : raw;
      const rows: PlainObject[] = Array.isArray(parsed) ? parsed : [];
      return ok(rows.map(mapResultDoc) as Document<S>[]);
    } catch (e: unknown) {
      return err(new Error(`find query failed: ${e instanceof Error ? e.message : String(e)}`, { cause: e }));
    }
  }
}
