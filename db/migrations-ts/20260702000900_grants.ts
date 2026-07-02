import { dropFunction, grant, revoke } from "@zeroship/migrate/pg";

export const name = "grants";

export function up() {
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["zeroship_auth", "zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_app"] });
  grant({ privileges: ["create", "usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_admin"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_app"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_audit"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_gdpr"] });
  grant({ privileges: ["usage"], on: { kind: "sequence", in: "zeroship" }, to: ["zeroship_auth", "zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["users", "idp_sessions", "magic_links", "magic_completions", "email_verifications", "totp_credentials", "totp_backup_codes", "oauth_refresh_tokens", "oauth_authorization_codes", "device_grants", "signing_keys", "oidc_session_clients"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["federated_identities", "jwk_key_state", "rate_limits", "audit_events"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["oauth_grants", "oauth_clients", "token_revocations"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["app_scope_defs"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["email_suppressions", "cron_state"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_user_identities", "app_session_anchors"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions", "app_user_identities"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_session_anchors"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["token_revocations"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["signing_keys"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["apps", "oauth_clients", "app_members", "app_secrets", "billing_metrics", "usage_aggregates", "creator_billing", "billing_customer_refs", "platform_policies", "platform_admin_roles", "invoice_lines", "app_net_grants", "identity_links", "principal_grants", "device_grants"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["creator_accounts", "creator_account_history", "permission_tokens", "metering_exports", "metric_weights", "pricing_config", "plans", "app_spend_limit", "app_spend_state", "invoices", "creator_fee_policy", "creator_billing_status", "refunds", "billing_notifications", "billing_disputes", "billing_reconciliation_findings", "net_policy_catalog", "migrated_migrations"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_vars", "app_env_expose", "oauth_grants", "app_scope_defs", "token_revocations", "usage_reports_seen", "spend_state_history", "pending_disputes", "billing_line_provider_refs"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["app_usage", "app_usage_history", "app_oauth_clients", "payouts", "invoice_payments", "creator_billing_status_history", "stripe_events_seen", "credit_ledger", "refund_provider_refs", "plan_change_events", "payout_failures", "billing_provider_refs", "migrated_app_policies", "migrated_migration_audit"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["users"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["rate_limits"] }, to: ["zeroship_control"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["app_audit"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_user_identities"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandboxes", "shares", "hosts", "deleted_sandboxes", "wake_jobs"] }, to: ["sandbox_app"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events"] }, to: ["sandbox_app"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events"] }, to: ["sandbox_audit"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandboxes", "shares"] }, to: ["sandbox_gdpr"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events", "deleted_sandboxes"] }, to: ["sandbox_gdpr"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, from: ["public"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["app_audit"] }, from: ["public"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["authz_decisions"] }, from: ["public"] });
  dropFunction({ schema: "zeroship", name: "zeroship_migrations_schema_migrations_immutable", ifExists: true });
}

export function down() {

}
