import { currentSetting, table } from "@zeroship/migrate";

export default {
  name: "policies_rls",
  schema() {
    table("app_secrets", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("app_session_anchors", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("app_spend_limit", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("app_spend_state", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("app_user_identities", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("gateway_sessions", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("plan_change_events", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("spend_state_history", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("usage_aggregates", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
    table("app_secrets", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("app_session_anchors", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("app_spend_limit", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("app_spend_state", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("app_user_identities", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })), withCheck: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })) });
    table("gateway_sessions", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("plan_change_events", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("spend_state_history", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
    table("usage_aggregates", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) });
  },
};
