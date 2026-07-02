import { table } from "@zeroship/migrate";

export const name = "policies_rls";

export function up() {
  table("app_secrets", { schema: "zeroship" }).enableRowLevelSecurity();
  table("app_session_anchors", { schema: "zeroship" }).enableRowLevelSecurity();
  table("app_spend_limit", { schema: "zeroship" }).enableRowLevelSecurity();
  table("app_spend_state", { schema: "zeroship" }).enableRowLevelSecurity();
  table("app_user_identities", { schema: "zeroship" }).enableRowLevelSecurity();
  table("gateway_sessions", { schema: "zeroship" }).enableRowLevelSecurity();
  table("plan_change_events", { schema: "zeroship" }).enableRowLevelSecurity();
  table("spend_state_history", { schema: "zeroship" }).enableRowLevelSecurity();
  table("usage_aggregates", { schema: "zeroship" }).enableRowLevelSecurity();
  table("app_secrets", { schema: "zeroship" }).forceRowLevelSecurity();
  table("app_session_anchors", { schema: "zeroship" }).forceRowLevelSecurity();
  table("app_spend_limit", { schema: "zeroship" }).forceRowLevelSecurity();
  table("app_spend_state", { schema: "zeroship" }).forceRowLevelSecurity();
  table("app_user_identities", { schema: "zeroship" }).forceRowLevelSecurity();
  table("gateway_sessions", { schema: "zeroship" }).forceRowLevelSecurity();
  table("plan_change_events", { schema: "zeroship" }).forceRowLevelSecurity();
  table("spend_state_history", { schema: "zeroship" }).forceRowLevelSecurity();
  table("usage_aggregates", { schema: "zeroship" }).forceRowLevelSecurity();
  table("app_secrets", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("app_session_anchors", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("app_spend_limit", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("app_spend_state", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("app_user_identities", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_client_id").eq(c.fn.currentSetting("zeroship.tenant_client", true)), withCheck: (c) => c("app_client_id").eq(c.fn.currentSetting("zeroship.tenant_client", true)) });
  table("gateway_sessions", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("plan_change_events", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("spend_state_history", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  table("usage_aggregates", { schema: "zeroship" }).createPolicy({ name: "tenant_isolation", using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
}

export function down() {

}
