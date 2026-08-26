import {
  table,
  t,
  currentSetting,
  extension,
  grant,
  role,
  schema,
} from "zero-migrate";

export const name = "platform_ts_vendor";

export function up() {
  extension("citext").create({ ifNotExists: true });
  schema("zero_migrate").create({ ifNotExists: true });
  role("zero_migrate_ts_test_app").create({
    login: true,
    password: "zero_migrate_ts_test_app",
    setSearchPath: ["zero_migrate", "public"],
    ifNotExists: true,
  });

  table("ts_accounts", { schema: "zero_migrate" }).create({
    columns: {
      id: t.bigInt().identity({ always: true }).primaryKey(),
      app_id: t.text().notNull(),
      email: t.text().notNull(),
    },
  });

  grant({
    privileges: ["select"],
    on: { kind: "table", names: ["ts_accounts"], schema: "zero_migrate" },
    to: ["zero_migrate_ts_test_app"],
  });

  const accounts = table("ts_accounts", { schema: "zero_migrate" });
  accounts.setRls({ enabled: true, forced: true });
  accounts.policy("tenant_isolation").create({
    for: "all",
    using: (col) =>
      col("app_id").eq(currentSetting("zero_migrate.tenant_app", { missingOk: true }).cast({ to: "text" })),
    withCheck: (col) =>
      col("app_id").eq(currentSetting("zero_migrate.tenant_app", { missingOk: true }).cast({ to: "text" })),
  });
}
