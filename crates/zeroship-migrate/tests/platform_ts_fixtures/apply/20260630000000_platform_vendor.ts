import { table, t } from "@zeroship/migrate";
import { pg } from "@zeroship/migrate/pg";

export const name = "platform_ts_vendor";

export function up() {
  pg.createExtension({ name: "citext", ifNotExists: true });
  pg.createSchema({ name: "zeroship", ifNotExists: true });
  pg.createRole({
    name: "zeroship_ts_test_app",
    login: true,
    password: "zeroship_ts_test_app",
    setSearchPath: ["zeroship", "public"],
    ifNotExists: true,
  });

  table("ts_accounts", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
      app_id: t.text().notNull(),
      email: t.text().notNull(),
    },
  });

  pg.grant({
    privileges: ["select"],
    on: { kind: "table", names: ["ts_accounts"], schema: "zeroship" },
    to: ["zeroship_ts_test_app"],
  });

  const accounts = table("ts_accounts", { schema: "zeroship" });
  accounts.enableRowLevelSecurity();
  accounts.forceRowLevelSecurity();
  accounts.createPolicy({
    name: "tenant_isolation",
    for: "all",
    using: (c) =>
      c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("text")),
    withCheck: (c) =>
      c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("text")),
  });
}
