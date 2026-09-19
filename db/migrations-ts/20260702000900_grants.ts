import { grant, raw, revoke } from "@zeroship/migrate";

// The grant state for the core `zeroship` tables. Domain migrations grant the
// tables they create, and a column-scoped grant lives with the domain that
// consumes the column. This file is the authority for the core table-level set
// and for the worker's deny boundary, whose raw islands carry the class-wide
// revokes the grant DSL cannot express.
export default {
  name: "grants",
  schema() {
    // ---- schema reach ------------------------------------------------------
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["zeroship_auth", "zeroship_control", "zeroship_gateway", "zeroship_app", "zeroship_cdc"] });
    grant({ privileges: ["usage"], on: { kind: "sequence", in: "zeroship" }, to: ["zeroship_auth", "zeroship_gateway"] });

    // ---- zeroship_auth -----------------------------------------------------
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["users", "idp_sessions", "magic_links", "magic_completions", "email_verifications", "totp_credentials", "totp_backup_codes", "oauth_authorization_codes", "device_grants", "signing_keys", "oidc_session_clients"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["rate_limits", "token_revocations"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["federated_identities", "audit_events"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["oauth_grants", "oauth_clients", "email_suppressions", "app_user_identities"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["app_scope_defs", "app_oauth_clients", "principal_grants"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["cron_state"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_session_anchors"] }, to: ["zeroship_auth"] });
    grant({ privileges: ["select", "delete"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions"] }, to: ["zeroship_auth"] });

    // ---- zeroship_gateway --------------------------------------------------
    grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["gateway_sessions", "app_user_identities"] }, to: ["zeroship_gateway"] });
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_session_anchors", "token_revocations"] }, to: ["zeroship_gateway"] });
    grant({ privileges: ["insert"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, to: ["zeroship_gateway"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["signing_keys"] }, to: ["zeroship_gateway"] });

    // ---- zeroship_control --------------------------------------------------
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["oauth_clients", "app_secrets", "billing_metrics", "usage_aggregates", "invoice_lines", "identity_links", "principal_grants", "device_grants", "app_vars", "token_revocations", "rate_limits"] }, to: ["zeroship_control"] });
    grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema: "zeroship", names: ["apps", "metric_weights", "pricing_config", "plans", "app_spend_limit", "app_spend_state", "invoices", "refunds", "billing_disputes", "billing_reconciliation_findings", "provider_dead_letter", "app_schema_applies", "app_oauth_clients"] }, to: ["zeroship_control"] });
    grant({ privileges: ["select", "insert", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_env_expose", "oauth_grants", "app_scope_defs", "spend_state_history", "pending_disputes", "billing_line_provider_refs", "app_audit", "authz_decisions"] }, to: ["zeroship_control"] });
    grant({ privileges: ["select", "insert"], on: { kind: "table", schema: "zeroship", names: ["app_usage", "app_usage_history", "payouts", "invoice_payments", "stripe_events_seen", "credit_ledger", "refund_provider_refs", "plan_change_events", "payout_failures", "billing_provider_refs", "connect_checkout_failures"] }, to: ["zeroship_control"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "zeroship", names: ["users"] }, to: ["zeroship_control"] });
    grant({ privileges: ["select", "update"], on: { kind: "table", schema: "zeroship", names: ["app_user_identities"] }, to: ["zeroship_control"] });

    // The append-only audit trail refuses UPDATE/DELETE/TRUNCATE to every path
    // the tamper trigger does not admit; this revoke is the floor beneath it.
    revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["audit_events"] }, from: ["public"] });
    revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["app_audit"] }, from: ["public"] });
    revoke({ privileges: ["update", "delete", "truncate"], on: { kind: "table", schema: "zeroship", names: ["authz_decisions"] }, from: ["public"] });

    // ---- the worker's boundary ---------------------------------------------
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
      reason: "explicit intent only - no default table grant exists to revoke today, so this stores nothing",
    });
    raw({
      sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON SEQUENCES FROM zeroship_worker",
      reason: "explicit intent only - no default sequence grant exists to revoke today, so this stores nothing",
    });
    revoke({ privileges: ["create"], on: { kind: "schema", names: ["zeroship"] }, from: ["zeroship_worker"] });
    revoke({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, from: ["zeroship_worker"] });
  },
};
