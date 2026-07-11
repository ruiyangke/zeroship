import { table, t, schema } from "@zeroship/migrate";

export const name = "platform_ts_first_ok";

export function up() {
  schema("zeroship").create({ ifNotExists: true });
  table("ts_first_ok", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
}
