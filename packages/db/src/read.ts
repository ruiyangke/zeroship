import type { AliasedCollection } from "./db-types";

/** Column reference inside a read projection/condition. */
export type ColumnNode = { source: string; field: string };
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

export type Projection = ReadRow<any, any, boolean> | ReadColumn<any, any> | ReadAggregate<any>;
export type Projected<P, Nullable extends string> = { [K in keyof P]: P[K] extends ReadColumn<infer T, infer A>
  ? A extends Nullable ? T | null : T : P[K] extends { readonly _value: infer T } ? T : never };
export type Allowed<P, Nullable extends string> = { [K in keyof P]: P[K] extends ReadRow<any, infer A, false> ? A extends Nullable ? never : P[K] : P[K] };
