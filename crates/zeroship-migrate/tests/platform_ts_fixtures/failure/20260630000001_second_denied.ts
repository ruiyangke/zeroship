import { table, t } from "@zeroship/migrate";
import { pg } from "@zeroship/migrate/pg";

export const name = "platform_ts_second_denied";

export function up() {
  table("ts_should_not_exist", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
    },
  });
  pg.raw({ sql: "ALTER SYSTEM SET log_min_duration_statement = '2s'" });
}
