// `schema` is aliased because this migration's forward phase is itself named
// `schema`. Importing the helper under its own name beside `export function
// schema()` is a duplicate lexical binding - a SyntaxError, not a shadow - so
// the file would not parse at all.
import { table, t, schema as declareSchema } from "@zeroship/migrate";

export const name = "platform_ts_first_ok";

export function schema() {
  declareSchema("zero_migrate").create({ ifNotExists: true });
  table("ts_first_ok", { schema: "zero_migrate" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
}
