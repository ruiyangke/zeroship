import { grant, raw, revoke } from "@zeroship/migrate";

export const name = "grants";

export function up() {
  // The worker streams creator-table WAL and owns only per-app workflow
  // journals. REPLICATION is required for the former; membership in the
  // NOLOGIN workflow owner is required for the latter. Neither capability
  // grants a write path into the platform schema.
  raw({
    sql: "ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT REPLICATION BYPASSRLS",
    reason: "the role DSL does not expose PostgreSQL's REPLICATION attribute",
  });
  raw({
    sql: "ALTER ROLE zeroship_workflow_owner WITH NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS",
    reason: "reassert the exact narrow attributes of the workflow journal owner",
  });
  raw({
    sql: "GRANT zeroship_workflow_owner TO zeroship_worker",
    reason: "reassert membership when platform roles predate a fresh migration run",
  });

  // Start from zero effective authority over every current and future platform
  // relation. Later grants give the worker only the columns its workflow
  // dispatcher reads. This schema-wide deny is the class boundary: it includes
  // device_grants and every other authorization-decision table without relying
  // on a hand-maintained sensitive-table list.
  raw({
    sql: "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker",
    reason: "the grant DSL has no ALL TABLES IN SCHEMA target",
  });
  raw({
    sql: "REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA zeroship FROM zeroship_worker",
    reason: "the worker must not advance platform-owned sequences",
  });
  raw({
    sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES FROM zeroship_worker",
    reason: "future platform tables must inherit the same deny-by-default boundary",
  });
  raw({
    sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON SEQUENCES FROM zeroship_worker",
    reason: "future platform sequences must inherit the same deny-by-default boundary",
  });
  revoke({ privileges: ["create"], on: { kind: "schema", names: ["zeroship"] }, from: ["zeroship_worker", "zeroship_workflow_owner"] });

  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["zeroship_auth", "zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_app"] });
  grant({ privileges: ["create", "usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_admin"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_app"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_audit"] });
  grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["sandbox_gdpr"] });
  grant({ privileges: ["usage"], on: { kind: "sequence", in: "zeroship" }, to: ["zeroship_auth", "zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["users", "idp_sessions", "magic_links", "magic_completions", "email_verifications", "totp_credentials", "totp_backup_codes", "oauth_refresh_tokens", "oauth_authorization_codes", "device_grants", "signing_keys", "oidc_session_clients"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["federated_identities", "jwk_key_state", "rate_limits", "audit_events"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["oauth_grants", "oauth_clients", "token_revocations"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["app_scope_defs", "principal_grants"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["email_suppressions", "cron_state"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_user_identities", "app_session_anchors"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions"] }, to: ["zeroship_auth"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions", "app_user_identities"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_session_anchors"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["token_revocations"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["signing_keys"] }, to: ["zeroship_gateway"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["apps", "oauth_clients", "app_members", "app_secrets", "billing_metrics", "usage_aggregates", "creator_billing", "billing_customer_refs", "platform_policies", "platform_admin_roles", "invoice_lines", "app_net_grants", "identity_links", "principal_grants", "device_grants"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["creator_accounts", "creator_account_history", "permission_tokens", "metering_exports", "metric_weights", "pricing_config", "plans", "app_spend_limit", "app_spend_state", "invoices", "creator_fee_policy", "creator_billing_status", "refunds", "billing_notifications", "billing_disputes", "billing_reconciliation_findings", "provider_dead_letter", "net_policy_catalog", "migrated_migrations"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_vars", "app_env_expose", "oauth_grants", "app_scope_defs", "token_revocations", "spend_state_history", "pending_disputes", "billing_line_provider_refs"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["app_usage", "app_usage_history", "app_oauth_clients", "payouts", "invoice_payments", "creator_billing_status_history", "stripe_events_seen", "credit_ledger", "refund_provider_refs", "plan_change_events", "payout_failures", "billing_provider_refs", "migrated_app_policies", "migrated_migration_audit"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["users"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["rate_limits"] }, to: ["zeroship_control"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["app_audit"] }, to: ["zeroship_control"] });
  grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_user_identities"] }, to: ["zeroship_control"] });
  raw({
    sql: "GRANT SELECT (id, plan_id, workflows_enabled) ON zeroship.apps TO zeroship_worker",
    reason: "workflow claims need only the app's plan and workflow enablement fields",
  });
  raw({
    sql: "GRANT SELECT (id, name, runtime_limits_json, workflows_allowed, archived) ON zeroship.plans TO zeroship_worker",
    reason: "workflow claims need only the plan fields that set execution limits and eligibility",
  });
  grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandboxes", "shares", "hosts", "deleted_sandboxes", "wake_jobs"] }, to: ["sandbox_app"] });
  grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events"] }, to: ["sandbox_app"] });
  grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events"] }, to: ["sandbox_audit"] });
  grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandboxes", "shares"] }, to: ["sandbox_gdpr"] });
  grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["sandbox_events", "deleted_sandboxes"] }, to: ["sandbox_gdpr"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, from: ["public"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["app_audit"] }, from: ["public"] });
  revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["authz_decisions"] }, from: ["public"] });
  // NOTE: the SQL baseline dropped `zeroship_migrations.schema_migrations_immutable()`
  // here (an artifact of the retired in-tree engine's journal bootstrap). The
  // published zero-migrate engine OWNS `<meta>_schema_migrations_immutable()` as its
  // LIVE journal tamper-guard (meta schema `zeroship_migrations`), with a dependent
  // BEFORE UPDATE/DELETE trigger. Dropping it would (a) fail without CASCADE and
  // (b) if forced, disarm the append-only journal protection — so this stale
  // cleanup line is removed; the engine manages its own journal function.
}

export function down() {

}
