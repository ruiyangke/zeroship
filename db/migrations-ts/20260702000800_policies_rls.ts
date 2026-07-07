import { currentSetting, pgTable } from "@zeroship/migrate";

export const name = "policies_rls";

export function up() {
  pgTable("app_secrets", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("app_session_anchors", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("app_spend_limit", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("app_spend_state", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("app_user_identities", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("gateway_sessions", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("plan_change_events", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("spend_state_history", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("usage_aggregates", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
  pgTable("app_secrets", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("app_session_anchors", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("app_spend_limit", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("app_spend_state", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("app_user_identities", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })), withCheck: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })) });
  pgTable("gateway_sessions", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("plan_change_events", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("spend_state_history", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  pgTable("usage_aggregates", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
}

export function down() {

}
