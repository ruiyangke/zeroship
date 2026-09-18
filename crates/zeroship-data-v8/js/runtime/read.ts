import type { NativeDb } from "../../../../packages/db/src/native";
import { err, ok, type PlainObject, type Result } from "../../../../packages/db/src/types";
import {
  ReadColumn,
  ReadRow,
  type Projection,
  type Projected,
  type Allowed,
  type ReadCondition,
  type ReadOrder,
} from "../../../../packages/db/src/read";
import type {
  AliasedCollection as AliasedCollectionContract,
  ReadBuilder as ReadBuilderContract,
} from "../../../../packages/db/src/db-types";
import { mapNativeError } from "./errors";
import { trackCollectionAccess } from "./live";
import { mapResultDoc } from "./utils";

type ColumnNode = { source: string; field: string };
type Expression = ColumnNode | { aggregate: string; column?: ColumnNode; distinct?: boolean };
const aliasScopes = new WeakMap<object, () => boolean>();

function aliasIsActive(source: object): boolean {
  return aliasScopes.get(source)?.() ?? true;
}

export class AliasedCollection<T, A extends string = string>
  implements AliasedCollectionContract<T, A> {
  readonly columns: { [K in keyof T]-?: ReadColumn<T[K], A> };
  constructor(readonly native: NativeDb, readonly collection: string, readonly alias: A,
    fields: readonly string[], readonly toColumn: (field: string) => string, readonly toField: (field: string) => string) {
    this.columns = Object.fromEntries(fields.map(field => [field, new ReadColumn({ source: alias, field: toColumn(field) })])) as typeof this.columns;
  }
  row(): ReadRow<T, A, false> { return new ReadRow(this, false); }
  optionalRow(): ReadRow<T, A, true> { return new ReadRow(this, true); }
}

export function scopeAliasedCollection<T, A extends string>(
  source: AliasedCollection<T, A>,
  active: () => boolean,
): AliasedCollection<T, A> {
  const scoped = new AliasedCollection<T, A>(
    source.native,
    source.collection,
    source.alias,
    Object.keys(source.columns),
    source.toColumn,
    source.toField,
  );
  aliasScopes.set(scoped, active);
  return scoped;
}

type Input = { from: { collection: string; alias: string }; joins: { kind: string; source: { collection: string; alias: string }; on: ReadCondition }[];
  select?: Record<string, unknown>; where?: ReadCondition; groupBy?: ColumnNode[]; having?: ReadCondition; orderBy?: ReadOrder[]; limit?: number; offset?: number };

export class ReadBuilder<P = never, Nullable extends string = never, Throws extends boolean = false>
  implements ReadBuilderContract<P, Nullable, Throws> {
  constructor(private readonly root: AliasedCollection<any>, private readonly input: Input,
    private readonly sources: readonly AliasedCollection<any>[], private readonly projection: Record<string, Projection>,
    private readonly throws: Throws, private readonly active: () => boolean) {}
  private copy<Q = P, N extends string = Nullable>(input: Input, sources = this.sources, projection = this.projection): ReadBuilder<Q, N, Throws> {
    return new ReadBuilder(this.root, input, sources, projection, this.throws, this.active);
  }
  private join<T, A extends string, N extends string>(kind: string, source: AliasedCollection<T, A>, on: ReadCondition): ReadBuilder<P, N, Throws> {
    if (source.native !== this.root.native) throw new Error("join sources must belong to the same database");
    if (this.sources.some(s => s.alias === source.alias)) throw new Error("duplicate join alias");
    return this.copy<P, N>({ ...this.input, joins: [...this.input.joins, { kind, source: { collection: source.collection, alias: source.alias }, on }] }, [...this.sources, source]);
  }
  innerJoin<T, A extends string>(source: AliasedCollection<T, A>, on: ReadCondition): ReadBuilder<P, Nullable, Throws> { return this.join("inner", source, on); }
  leftJoin<T, A extends string>(source: AliasedCollection<T, A>, on: ReadCondition): ReadBuilder<P, Nullable | A, Throws> { return this.join("left", source, on); }
  where(where: ReadCondition): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, where }); }
  having(having: ReadCondition): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, having }); }
  groupBy(...columns: ReadColumn<unknown>[]): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, groupBy: columns.map(c => c.expression) }); }
  orderBy(...keys: ReadOrder[]): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, orderBy: keys }); }
  limit(limit: number): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, limit }); }
  offset(offset: number): ReadBuilder<P, Nullable, Throws> { return this.copy({ ...this.input, offset }); }
  select<const Q extends Record<string, Projection>>(projection: Q & Allowed<Q, Nullable>): ReadBuilder<Projected<Q, Nullable>, Nullable, Throws> {
    const select: Record<string, unknown> = Object.create(null);
    for (const [name, item] of Object.entries(projection)) {
      select[name] = item.kind === "row" ? { row: item.source.alias, optional: item.optional, ...(item.fields ? { fields: item.fields.map(item.source.toColumn) } : {}) } : item.expression;
    }
    return this.copy({ ...this.input, select }, this.sources, projection);
  }
  all(): Promise<Throws extends true ? P[] : Result<P[]>> {
    let work: Promise<Record<string, unknown>[]>;
    try {
      if (!this.active() || this.sources.some(source => !aliasIsActive(source))) {
        throw Object.assign(new Error("transaction scope has expired"), {
          code: "TRANSACTION_SCOPE_EXPIRED" as const,
        });
      }
      if (!this.input.select) throw new Error("read requires an explicit projection");
      for (const source of this.sources) trackCollectionAccess(source.collection);
      work = this.root.native.collection(this.root.collection).read(this.input);
    } catch (error) { work = Promise.reject(error); }
    return work.then(rows => {
      const mapped = rows.map(row => {
        const output: PlainObject = {};
        for (const [name, projection] of Object.entries(this.projection)) {
          const value = row[name];
          output[name] = projection.kind === "row" && value !== null ? mapResultDoc(value as PlainObject, projection.source.toField) : value;
        }
        return output as P;
      });
      return this.throws ? mapped : ok(mapped);
    }).catch(failure => {
      const error = mapNativeError(failure);
      if (this.throws) throw error;
      return err(error);
    }) as Promise<Throws extends true ? P[] : Result<P[]>>;
  }
}

export function readFrom<T, A extends string, Throws extends boolean = false>(native: NativeDb, source: AliasedCollection<T, A>, throws = false as Throws, active: () => boolean = () => true): ReadBuilder<never, never, Throws> {
  if (source.native !== native) throw new Error("read source belongs to another database");
  return new ReadBuilder(source, { from: { collection: source.collection, alias: source.alias }, joins: [] }, [source], {}, throws, () => active() && aliasIsActive(source));
}
