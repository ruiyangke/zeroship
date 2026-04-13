import { mapResultDoc } from "./utils.js";

type PlainObject = Record<string, unknown>;

type NativeFn = (
  collection: string,
  filter: PlainObject,
  opts: PlainObject
) => Promise<string>;

export class Query {
  private _collection: string;
  private _filter: PlainObject;
  private _native: NativeFn;

  private _sort: PlainObject | undefined;
  private _limit: number | undefined;
  private _skip: number | undefined;
  private _select: string[] | undefined;

  constructor(
    collection: string,
    filter: PlainObject,
    native: NativeFn
  ) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
  }

  sort(obj: PlainObject): this {
    this._sort = obj;
    return this;
  }

  limit(n: number): this {
    this._limit = n;
    return this;
  }

  skip(n: number): this {
    this._skip = n;
    return this;
  }

  select(s: string | string[]): this {
    if (Array.isArray(s)) {
      this._select = s;
    } else {
      this._select = s.split(" ").filter((f) => f.length > 0);
    }
    return this;
  }

  then(
    resolve: (value: PlainObject[]) => unknown,
    reject: (reason: unknown) => unknown
  ): Promise<unknown> {
    return this._exec().then(resolve, reject);
  }

  async _exec(): Promise<PlainObject[]> {
    const opts: PlainObject = {};
    if (this._sort !== undefined) opts["sort"] = this._sort;
    if (this._limit !== undefined) opts["limit"] = this._limit;
    if (this._skip !== undefined) opts["skip"] = this._skip;
    if (this._select !== undefined) opts["select"] = this._select;

    const raw = await this._native(this._collection, this._filter, opts);
    const rows: PlainObject[] = typeof raw === "string" ? JSON.parse(raw) : (raw as unknown as PlainObject[]);
    return rows.map(mapResultDoc);
  }
}
