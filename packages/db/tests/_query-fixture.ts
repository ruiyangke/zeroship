import { Query } from "../src/query.js";
import type { PlainObject, Row } from "../src/types.js";

/** Query fixtures use mock rows keyed by the explicitly declared id field. */
export class FixtureQuery<S = PlainObject, P = Row<S>, AllSchemas extends Record<string, unknown> = Record<string, unknown>> extends Query<S, P, AllSchemas> {
  constructor(...args: ConstructorParameters<typeof Query<S, P, AllSchemas>>) {
    args[7] ??= { id: { type: "string", primaryKey: true } };
    super(...args);
  }
}
