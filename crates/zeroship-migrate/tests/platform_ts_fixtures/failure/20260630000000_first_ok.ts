import { table, t, schema } from "zero-migrate";

export const name = "platform_ts_first_ok";

export function up() {
  schema("zero_migrate").create({ ifNotExists: true });
  table("ts_first_ok", { schema: "zero_migrate" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
}
