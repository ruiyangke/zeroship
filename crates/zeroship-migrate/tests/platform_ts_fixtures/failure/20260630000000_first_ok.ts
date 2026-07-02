import { table, t } from "@zeroship/migrate";
import { pg } from "@zeroship/migrate/pg";

export const name = "platform_ts_first_ok";

export function up() {
  pg.createSchema({ name: "zeroship", ifNotExists: true });
  table("ts_first_ok", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
}
