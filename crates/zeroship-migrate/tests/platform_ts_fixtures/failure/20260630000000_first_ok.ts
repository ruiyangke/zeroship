import { table, t } from "@zeroship/migrate";
import { schema } from "@zeroship/migrate/pg";

export const name = "platform_ts_first_ok";

export function up() {
  schema({ name: "zeroship", ifNotExists: true });
  table("ts_first_ok", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
}
