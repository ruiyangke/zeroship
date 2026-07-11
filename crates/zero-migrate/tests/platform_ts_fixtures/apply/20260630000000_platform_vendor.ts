import {
  table,
  t,
  currentSetting,
  extension,
  grant,
  role,
  schema,
} from "@zeroship/migrate";

export const name = "platform_ts_vendor";

export function up() {
  extension("citext").create({ ifNotExists: true });
  schema("zeroship").create({ ifNotExists: true });
  role("zeroship_ts_test_app").create({
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

  const accounts = table("ts_accounts", { schema: "zeroship" });
  accounts.setRls({ enabled: true, forced: true });
  accounts.policy("tenant_isolation").create({
    for: "all",
    using: (col) =>
      col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "text" })),
    withCheck: (col) =>
      col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "text" })),
  });
}
