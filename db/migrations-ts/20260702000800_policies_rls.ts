import { pgTable } from "@zeroship/migrate/pg";

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
  pgTable("app_secrets", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_session_anchors", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_spend_limit", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_spend_state", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("app_user_identities", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_client_id").eq(c.pg.currentSetting("zeroship.tenant_client", true)), withCheck: (c) => c("app_client_id").eq(c.pg.currentSetting("zeroship.tenant_client", true)) });
  pgTable("gateway_sessions", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("plan_change_events", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("spend_state_history", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
  pgTable("usage_aggregates", { schema: "zeroship" }).policy("tenant_isolation").create({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")), withCheck: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")) });
}

export function down() {

}
