import { currentSetting, table } from "@zeroship/migrate";

// Every tenant predicate here compares a typed-id text column against
// `current_setting`, which is text. NO CAST BELONGS ON EITHER SIDE. A cast to
// uuid would not merely be redundant: an `app_<base62>` value raises
// `invalid input syntax for type uuid` inside the policy, and a policy that
// raises is not a policy that denies - the statement errors instead of
// returning no rows, so a broken predicate reads as an outage rather than as a
// silently open table. Keep both sides text.
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
    table("app_secrets", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("app_session_anchors", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("app_spend_limit", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("app_spend_state", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("app_user_identities", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })), withCheck: (col) => col("app_client_id").eq(currentSetting("zeroship.tenant_client", { missingOk: true })) });
    table("gateway_sessions", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("plan_change_events", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("spend_state_history", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
    table("usage_aggregates", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })), withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app", { missingOk: true })) });
  },
};
