import { table, t } from "@zeroship/migrate";
import { extension, grant, pgTable, role, schema } from "@zeroship/migrate/pg";

export const name = "platform_ts_vendor";

export function up() {
  extension({ name: "citext", ifNotExists: true });
  schema({ name: "zeroship", ifNotExists: true });
  role({
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

  grant({
    privileges: ["select"],
    on: { kind: "table", names: ["ts_accounts"], schema: "zeroship" },
    to: ["zeroship_ts_test_app"],
  });

  const accounts = pgTable("ts_accounts", { schema: "zeroship" });
  accounts.setRls({ enabled: true, forced: true });
  accounts.policy("tenant_isolation").create({
    for: "all",
    using: (c) =>
      c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("text")),
    withCheck: (c) =>
      c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("text")),
  });
}
