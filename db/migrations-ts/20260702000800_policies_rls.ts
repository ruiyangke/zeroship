import { raw } from "@zeroship/migrate/pg";

export const name = "policies_rls";

export function up() {
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.app_session_anchors ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.app_spend_limit ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.app_spend_state ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.app_user_identities ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.gateway_sessions ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.plan_change_events ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY", reason: "enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.app_secrets FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_limit FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.app_user_identities FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates FORCE ROW LEVEL SECURITY", reason: "forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.app_secrets USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.app_session_anchors USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.app_spend_limit USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.app_spend_state USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.app_user_identities USING ((app_client_id = current_setting('zeroship.tenant_client'::text, true))) WITH CHECK ((app_client_id = current_setting('zeroship.tenant_client'::text, true)))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.gateway_sessions USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.plan_change_events USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.spend_state_history USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
  // TODO(dsl-v2): allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration
  raw({ sql: "CREATE POLICY tenant_isolation ON zeroship.usage_aggregates USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))", reason: "createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry" });
}

export function down() {

}
