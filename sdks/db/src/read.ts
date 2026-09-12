import { mapNativeError } from "./errors";
import { trackCollectionAccess } from "./live";
import type { NativeDb } from "./native";
import { err, ok, type PlainObject, type Result } from "./types";
import { mapResultDoc } from "./utils";

type ColumnNode = { source: string; field: string };
type Expression = ColumnNode | { aggregate: string; column?: ColumnNode; distinct?: boolean };
export type ReadCondition = { op: string; left?: Expression; right?: Expression | { value: unknown }; args?: ReadCondition[]; arg?: ReadCondition | Expression };
export type ReadOrder = { column: ColumnNode; direction: "asc" | "desc"; nulls: "first" | "last" };

export class ReadColumn<T, A extends string = string> {
  declare readonly _value: T;
  declare readonly _source: A;
  readonly kind = "column";
  constructor(readonly expression: ColumnNode) {}
  asc(): ReadOrder { return { column: this.expression, direction: "asc", nulls: "last" }; }
  desc(): ReadOrder { return { column: this.expression, direction: "desc", nulls: "first" }; }
}
export class ReadAggregate<T> {
  declare readonly _value: T;
  readonly kind = "aggregate";
  constructor(readonly expression: Expression) {}
}
type Scalar<T = unknown> = ReadColumn<T, any> | ReadAggregate<T>;
export type ReadFrom<Throws extends boolean = false> = <T, A extends string>(source: AliasedCollection<T, A>) => ReadBuilder<never, never, Throws>;
function comparison<T>(op: string, left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition {
  if (right === null) {
    if (op !== "eq" && op !== "ne") throw new Error("null only supports equality comparisons");
    return { op: op === "eq" ? "isNull" : "isNotNull", arg: left.expression };
  }
  const expression = typeof right === "object" && right !== null && "kind" in right && (right.kind === "column" || right.kind === "aggregate")
    ? (right as Scalar).expression : { value: right };
  return { op, left: left.expression, right: expression };
}
export const eq = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("eq", left, right);
export const ne = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("ne", left, right);
export const gt = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("gt", left, right);
export const gte = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("gte", left, right);
export const lt = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("lt", left, right);
export const lte = <T>(left: Scalar<T>, right: Scalar<NoInfer<T>> | NoInfer<T>): ReadCondition => comparison("lte", left, right);
export const and = (...args: ReadCondition[]): ReadCondition => ({ op: "and", args });
export const or = (...args: ReadCondition[]): ReadCondition => ({ op: "or", args });
export const not = (arg: ReadCondition): ReadCondition => ({ op: "not", arg });
export const isNull = (arg: Scalar): ReadCondition => ({ op: "isNull", arg: arg.expression });
export const count = (column?: ReadColumn<unknown>, distinct = false): ReadAggregate<number> => new ReadAggregate({ aggregate: "count", ...(column ? { column: column.expression } : {}), distinct });
export const sum = (column: ReadColumn<number | null>): ReadAggregate<number | null> => new ReadAggregate({ aggregate: "sum", column: column.expression });
export const avg = (column: ReadColumn<number | null>): ReadAggregate<number | null> => new ReadAggregate({ aggregate: "avg", column: column.expression });
export const min = <T>(column: ReadColumn<T>): ReadAggregate<T | null> => new ReadAggregate({ aggregate: "min", column: column.expression });
export const max = <T>(column: ReadColumn<T>): ReadAggregate<T | null> => new ReadAggregate({ aggregate: "max", column: column.expression });

export class ReadRow<T, A extends string, Optional extends boolean> {
  declare readonly _value: Optional extends true ? T | null : T;
  declare readonly _source: A;
  readonly kind = "row";
  constructor(readonly source: AliasedCollection<T, A>, readonly optional: Optional, readonly fields?: string[]) {}
}
export class AliasedCollection<T, A extends string = string> {
  readonly columns: { [K in keyof T]-?: ReadColumn<T[K], A> };
  constructor(readonly native: NativeDb, readonly collection: string, readonly alias: A,
    fields: readonly string[], readonly toColumn: (field: string) => string, readonly toField: (field: string) => string) {
    this.columns = Object.fromEntries(fields.map(field => [field, new ReadColumn({ source: alias, field: toColumn(field) })])) as typeof this.columns;
  }
  row(): ReadRow<T, A, false> { return new ReadRow(this, false); }
  optionalRow(): ReadRow<T, A, true> { return new ReadRow(this, true); }
}
type Projection = ReadRow<any, any, boolean> | ReadColumn<any, any> | ReadAggregate<any>;
type Projected<P, Nullable extends string> = { [K in keyof P]: P[K] extends ReadColumn<infer T, infer A>
  ? A extends Nullable ? T | null : T : P[K] extends { readonly _value: infer T } ? T : never };
type Allowed<P, Nullable extends string> = { [K in keyof P]: P[K] extends ReadRow<any, infer A, false> ? A extends Nullable ? never : P[K] : P[K] };

type Input = { from: { collection: string; alias: string }; joins: { kind: string; source: { collection: string; alias: string }; on: ReadCondition }[];
  select?: Record<string, unknown>; where?: ReadCondition; groupBy?: ColumnNode[]; having?: ReadCondition; orderBy?: ReadOrder[]; limit?: number; offset?: number };

export class ReadBuilder<P = never, Nullable extends string = never, Throws extends boolean = false> {
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
      if (!this.active()) throw Object.assign(new Error("transaction scope has expired"), { code: "transaction_scope_expired" });
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
  return new ReadBuilder(source, { from: { collection: source.collection, alias: source.alias }, joins: [] }, [source], {}, throws, active);
}
