import { pgTable } from "@zeroship/migrate/pg";

export const name = "policies_rls";

export function up() {
  pgTable("app_secrets", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("app_session_anchors", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("app_spend_limit", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("app_spend_state", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("app_user_identities", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("gateway_sessions", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("plan_change_events", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("spend_state_history", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("usage_aggregates", { schema: "zeroship" }).enableRowLevelSecurity();
  pgTable("app_secrets", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("app_session_anchors", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("app_spend_limit", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("app_spend_state", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("app_user_identities", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("gateway_sessions", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("plan_change_events", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("spend_state_history", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("usage_aggregates", { schema: "zeroship" }).forceRowLevelSecurity();
  pgTable("app_secrets", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_session_anchors", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_spend_limit", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_spend_state", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_user_identities", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_client_id").eq(c.fn.currentSetting("zeroship.tenant_client", true)), withCheck: (c) => c("app_client_id").eq(c.fn.currentSetting("zeroship.tenant_client", true)) });
  pgTable("gateway_sessions", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("plan_change_events", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("spend_state_history", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("usage_aggregates", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
}

export function down() {

}
