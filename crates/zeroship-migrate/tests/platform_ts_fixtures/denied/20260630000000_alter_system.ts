import { pg } from "@zeroship/migrate/pg";

export const name = "platform_ts_denied_host_reach";

export function up() {
  pg.raw({ sql: "ALTER SYSTEM SET log_min_duration_statement = '1s'" });
}
