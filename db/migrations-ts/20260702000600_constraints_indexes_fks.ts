import { raw, table } from "@zeroship/migrate";

export default {
  name: "constraints_indexes_fks",
  schema() {
    table("app_oauth_clients", { schema: "zeroship" }).unique("app_oauth_clients_client_id_key").add({ columns: ["client_id"] });
    table("app_usage_history", { schema: "zeroship" }).unique("app_usage_history_app_id_period_key").add({ columns: ["app_id", "period"] });
    table("apps", { schema: "zeroship" }).unique("apps_name_key").add({ columns: ["name"] });
    table("billing_disputes", { schema: "zeroship" }).unique("billing_disputes_provider_dispute_id_key").add({ columns: ["provider_dispute_id"] });
    table("billing_line_provider_refs", { schema: "zeroship" }).unique("billing_line_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
    table("billing_provider_refs", { schema: "zeroship" }).unique("billing_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
    table("billing_reconciliation_findings", { schema: "zeroship" }).unique("billing_reconciliation_findings_dedup_key_key").add({ columns: ["dedup_key"] });
    table("connect_checkout_failures", { schema: "zeroship" }).unique("connect_checkout_failures_provider_payment_intent_id_key").add({ columns: ["provider_payment_intent_id"] });
    table("device_grants", { schema: "zeroship" }).unique("device_grants_user_code_key").add({ columns: ["user_code"] });
    table("federated_identities", { schema: "zeroship" }).unique("federated_identities_provider_subject_key").add({ columns: ["provider", "subject"] });
    table("payout_failures", { schema: "zeroship" }).unique("payout_failures_provider_payout_id_key").add({ columns: ["provider_payout_id"] });
    table("payouts", { schema: "zeroship" }).unique("payouts_event_id_key").add({ columns: ["event_id"] });
    table("provider_dead_letter", { schema: "zeroship" }).unique("provider_dead_letter_provider_event_key").add({ columns: ["provider_id", "event_id"] });
    table("refund_provider_refs", { schema: "zeroship" }).unique("refund_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
    table("refunds", { schema: "zeroship" }).unique("refunds_idempotency_key_key").add({ columns: ["idempotency_key"] });
    table("users", { schema: "zeroship" }).unique("users_email_key").add({ columns: ["email"] });
    table("app_session_anchors", { schema: "zeroship" }).index("app_session_anchors_user_idx").add({ on: ["app_id", "global_user_id"], where: (col) => col("revoked_at").isNull() });
    table("app_user_identities", { schema: "zeroship" }).index("app_user_identities_global_user_id_idx").add({ on: ["global_user_id"] });
    table("app_user_identities", { schema: "zeroship" }).index("app_user_identities_pairwise_sub_idx").add({ on: ["pairwise_sub"] });
    table("app_user_identities", { schema: "zeroship" }).index("app_user_identities_relay_active_idx").add({ on: ["relay_email"], unique: true, where: (col) => col("relay_email").isNotNull().and(col("revoked_at").isNull()) });
    table("apps", { schema: "zeroship" }).index("apps_organization_id_idx").add({ on: ["organization_id"] });
    table("apps", { schema: "zeroship" }).index("apps_plan_id_idx").add({ on: ["plan_id"] });
    table("apps", { schema: "zeroship" }).index("apps_project_organization_idx").add({ on: ["project_id", "organization_id"] });
    table("audit_events", { schema: "zeroship" }).index("auth_audit_event_idx").add({ on: ["event_type", "occurred_at"] });
    table("audit_events", { schema: "zeroship" }).index("auth_audit_user_idx").add({ on: ["actor_user_id", "occurred_at"] });
    table("dpop_jti", { schema: "zeroship" }).index("auth_dpop_jti_inserted_idx").add({ on: ["inserted_at"] });
    table("gateway_sessions", { schema: "zeroship" }).index("auth_gateway_sessions_app_idx").add({ on: ["app_id", "user_id"] });
    table("gateway_sessions", { schema: "zeroship" }).index("auth_gateway_sessions_app_sid_idx").add({ on: ["app_id", "sid"], where: (col) => col("sid").isNotNull().and(col("revoked_at").isNull()) });
    table("gateway_sessions", { schema: "zeroship" }).index("auth_gateway_sessions_idle_idx").add({ on: ["idle_expires_at"], where: (col) => col("revoked_at").isNull() });
    table("magic_completions", { schema: "zeroship" }).index("auth_magic_completions_expires_idx").add({ on: ["expires_at"], where: (col) => col("consumed_at").isNull() });
    table("magic_links", { schema: "zeroship" }).index("auth_magic_email_idx").add({ on: ["email"] });
    table("magic_links", { schema: "zeroship" }).index("auth_magic_user_id_idx").add({ on: ["user_id"] });
    table("rate_limits", { schema: "zeroship" }).index("auth_rate_limits_updated_at_idx").add({ on: ["updated_at"] });
    table("token_revocations", { schema: "zeroship" }).index("auth_token_revocations_revoked_after_idx").add({ on: ["revoked_after"] });
    table("totp_backup_codes", { schema: "zeroship" }).index("auth_totp_backup_codes_user_idx").add({ on: ["user_id"] });
    table("users", { schema: "zeroship" }).index("auth_users_deletion_due_idx").add({ on: ["deletion_scheduled_for"], where: (col) => col("deletion_scheduled_for").isNotNull().and(col("anonymized_at").isNull()) });
    table("users", { schema: "zeroship" }).index("auth_users_non_authenticating_idx").add({ on: ["id"], where: (col) => col("disabled_at").isNotNull().or(col("anonymized_at").isNotNull(), col("deletion_requested_at").isNotNull(), col("deletion_scheduled_for").isNotNull()) });
    table("authz_decisions", { schema: "zeroship" }).index("authz_decisions_occurred_idx").add({ on: [{ column: "occurred_at", order: "desc" }] });
    table("authz_decisions", { schema: "zeroship" }).index("authz_decisions_user_idx").add({ on: ["actor_user_id"], where: (col) => col("actor_user_id").isNotNull() });
    table("billing_disputes", { schema: "zeroship" }).index("billing_disputes_invoice_idx").add({ on: ["invoice_id"] });
    table("billing_metrics", { schema: "zeroship" }).index("billing_metrics_owner_app_idx").add({ on: ["owner_app"], where: (col) => col("owner_app").isNotNull() });
    table("billing_reconciliation_findings", { schema: "zeroship" }).index("billing_reconciliation_findings_kind_idx").add({ on: ["kind", "detected_at"] });
    table("billing_reconciliation_findings", { schema: "zeroship" }).index("billing_reconciliation_findings_open_idx").add({ on: ["detected_at"], where: (col) => col("resolved_at").isNull() });
    table("credit_ledger", { schema: "zeroship" }).index("credit_ledger_idempotency_key_idx").add({ on: ["idempotency_key"], unique: true, where: (col) => col("idempotency_key").isNotNull() });
    table("credit_ledger", { schema: "zeroship" }).index("credit_ledger_refund_clawback_note_idx").add({ on: ["note"], unique: true, where: (col) => col("kind").cast({ to: "text" }).eq("refund_clawback") });
    table("credit_ledger", { schema: "zeroship" }).index("credit_ledger_refund_to_credit_note_idx").add({ on: ["note"], unique: true, where: (col) => col("kind").cast({ to: "text" }).eq("refund_to_credit") });
    table("device_grants", { schema: "zeroship" }).index("device_grants_client_id_idx").add({ on: ["client_id"], where: (col) => col("client_id").isNotNull() });
    table("device_grants", { schema: "zeroship" }).index("device_grants_expires_at_idx").add({ on: ["expires_at"] });
    table("device_grants", { schema: "zeroship" }).index("device_grants_provider_pending_user_code_idx").add({ on: ["provider", "user_code"], where: (col) => col("status").eq("pending") });
    table("identity_links", { schema: "zeroship" }).index("identity_links_principal_id_idx").add({ on: ["principal_id"] });
    table("app_audit", { schema: "zeroship" }).index("idx_app_audit_app_at").add({ on: ["app_id", { column: "occurred_at", order: "desc" }] });
    table("provider_dead_letter", { schema: "zeroship" }).index("idx_provider_dead_letter_provider_created").add({ on: ["provider_id", { column: "created_at", order: "desc" }] });
    table("spend_state_history", { schema: "zeroship" }).index("idx_spend_state_history_app_at").add({ on: ["app_id", { column: "at", order: "desc" }] });
    table("spend_state_history", { schema: "zeroship" }).index("idx_spend_state_history_period").add({ on: ["period"] });
    table("invoice_lines", { schema: "zeroship" }).index("invoice_lines_correction_dedup_key_key").add({ on: ["correction_dedup_key"], unique: true, where: (col) => col("correction_dedup_key").isNotNull() });
    table("invoice_payments", { schema: "zeroship" }).index("invoice_payments_charge_provider_ref_key").add({ on: ["invoice_id", "provider_ref"], unique: true, where: (col) => col("kind").cast({ to: "text" }).eq("charge") });
    table("invoice_payments", { schema: "zeroship" }).index("invoice_payments_dispute_provider_ref_key").add({ on: ["invoice_id", "provider_ref", "kind"], unique: true, where: (col) => col("kind").cast({ to: "text" }).in(["dispute_debit", "dispute_reversal"]) });
    table("invoice_payments", { schema: "zeroship" }).index("invoice_payments_invoice_idx").add({ on: ["invoice_id"] });
    table("oauth_authorization_codes", { schema: "zeroship" }).index("oauth_authorization_codes_expires_at_idx").add({ on: ["expires_at"] });
    table("oauth_grants", { schema: "zeroship" }).index("oauth_grants_client_idx").add({ on: ["client_id"] });
    table("oauth_grants", { schema: "zeroship" }).index("oauth_grants_user_granted_idx").add({ on: ["user_id", { column: "granted_at", order: "desc" }] });
    table("oidc_session_clients", { schema: "zeroship" }).index("oidc_session_clients_client_idx").add({ on: ["client_id"] });
    table("oidc_session_clients", { schema: "zeroship" }).index("oidc_session_clients_user_idx").add({ on: ["user_id", "idp_session_id"] });
    table("pending_disputes", { schema: "zeroship" }).index("pending_disputes_charge_idx").add({ on: ["charge"], where: (col) => col("charge").isNotNull() });
    table("pending_disputes", { schema: "zeroship" }).index("pending_disputes_payment_intent_idx").add({ on: ["payment_intent"], where: (col) => col("payment_intent").isNotNull() });
    table("plan_change_events", { schema: "zeroship" }).index("plan_change_events_app_period_idx").add({ on: ["app_id", "period"] });
    table("refunds", { schema: "zeroship" }).index("refunds_invoice_idx").add({ on: ["invoice_id"] });
    table("signing_keys", { schema: "zeroship" }).index("signing_keys_status_idx").add({ on: ["status"] });
    table("usage_aggregates", { schema: "zeroship" }).index("usage_aggregates_period_idx").add({ on: ["period", "app_id"] });
    table("app_env_expose", { schema: "zeroship" }).foreignKey("app_env_expose_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_oauth_clients", { schema: "zeroship" }).foreignKey("app_oauth_clients_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_oauth_clients", { schema: "zeroship" }).foreignKey("app_oauth_clients_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("app_scope_defs", { schema: "zeroship" }).foreignKey("app_scope_defs_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_secrets", { schema: "zeroship" }).foreignKey("app_secrets_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_session_anchors", { schema: "zeroship" }).foreignKey("app_session_anchors_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_session_anchors", { schema: "zeroship" }).foreignKey("app_session_anchors_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("app_session_anchors", { schema: "zeroship" }).foreignKey("app_session_anchors_global_user_id_fkey").add({ columns: ["global_user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("app_spend_limit", { schema: "zeroship" }).foreignKey("app_spend_limit_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_spend_state", { schema: "zeroship" }).foreignKey("app_spend_state_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_usage", { schema: "zeroship" }).foreignKey("app_usage_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_usage_history", { schema: "zeroship" }).foreignKey("app_usage_history_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_user_identities", { schema: "zeroship" }).foreignKey("app_user_identities_app_client_id_fkey").add({ columns: ["app_client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("app_user_identities", { schema: "zeroship" }).foreignKey("app_user_identities_global_user_id_fkey").add({ columns: ["global_user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("app_vars", { schema: "zeroship" }).foreignKey("app_vars_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("apps", { schema: "zeroship" }).foreignKey("apps_plan_fk").add({ columns: ["plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
    table("billing_disputes", { schema: "zeroship" }).foreignKey("billing_disputes_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
    table("billing_line_provider_refs", { schema: "zeroship" }).foreignKey("billing_line_provider_refs_line_fk").add({ columns: ["invoice_id", "app_id", "segment_no"], references: { table: "invoice_lines", columns: ["invoice_id", "app_id", "segment_no"], schema: "zeroship" }, onDelete: "cascade" });
    table("billing_metrics", { schema: "zeroship" }).foreignKey("billing_metrics_owner_app_fkey").add({ columns: ["owner_app"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("billing_provider_refs", { schema: "zeroship" }).foreignKey("billing_provider_refs_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "cascade" });
    table("credit_ledger", { schema: "zeroship" }).foreignKey("credit_ledger_applied_invoice_id_fkey").add({ columns: ["applied_invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
    table("credit_ledger", { schema: "zeroship" }).foreignKey("credit_ledger_consumed_from_grant_id_fkey").add({ columns: ["consumed_from_grant_id"], references: { table: "credit_ledger", columns: ["id"] }, onDelete: "restrict" });
    table("device_grants", { schema: "zeroship" }).foreignKey("device_grants_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("device_grants", { schema: "zeroship" }).foreignKey("device_grants_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("email_verifications", { schema: "zeroship" }).foreignKey("email_verifications_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("federated_identities", { schema: "zeroship" }).foreignKey("federated_identities_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("gateway_sessions", { schema: "zeroship" }).foreignKey("gateway_sessions_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("identity_links", { schema: "zeroship" }).foreignKey("identity_links_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("idp_sessions", { schema: "zeroship" }).foreignKey("idp_sessions_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "restrict" });
    table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
    table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_plan_id_fkey").add({ columns: ["plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
    table("invoice_payments", { schema: "zeroship" }).foreignKey("invoice_payments_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
    table("magic_links", { schema: "zeroship" }).foreignKey("magic_links_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("metric_weights", { schema: "zeroship" }).foreignKey("metric_weights_metric_fkey").add({ columns: ["metric"], references: { table: "billing_metrics", columns: ["metric"], schema: "zeroship" }, onDelete: "restrict" });
    table("oauth_authorization_codes", { schema: "zeroship" }).foreignKey("oauth_authorization_codes_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("oauth_authorization_codes", { schema: "zeroship" }).foreignKey("oauth_authorization_codes_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("oauth_clients", { schema: "zeroship" }).foreignKey("oauth_clients_created_by_fkey").add({ columns: ["created_by"], references: { table: "users", columns: ["id"] }, onDelete: "setNull" });
    table("oauth_grants", { schema: "zeroship" }).foreignKey("oauth_grants_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("oauth_grants", { schema: "zeroship" }).foreignKey("oauth_grants_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("oidc_session_clients", { schema: "zeroship" }).foreignKey("oidc_session_clients_client_id_fkey").add({ columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
    table("oidc_session_clients", { schema: "zeroship" }).foreignKey("oidc_session_clients_idp_session_id_fkey").add({ columns: ["idp_session_id"], references: { table: "idp_sessions", columns: ["id"] }, onDelete: "cascade" });
    table("oidc_session_clients", { schema: "zeroship" }).foreignKey("oidc_session_clients_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_from_plan_id_fkey").add({ columns: ["from_plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
    table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_to_plan_id_fkey").add({ columns: ["to_plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
    table("principal_grants", { schema: "zeroship" }).foreignKey("principal_grants_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("refund_provider_refs", { schema: "zeroship" }).foreignKey("refund_provider_refs_refund_id_fkey").add({ columns: ["refund_id"], references: { table: "refunds", columns: ["id"] }, onDelete: "cascade" });
    table("refunds", { schema: "zeroship" }).foreignKey("refunds_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
    table("spend_state_history", { schema: "zeroship" }).foreignKey("spend_state_history_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("totp_backup_codes", { schema: "zeroship" }).foreignKey("totp_backup_codes_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("totp_credentials", { schema: "zeroship" }).foreignKey("totp_credentials_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
    table("usage_aggregates", { schema: "zeroship" }).foreignKey("usage_aggregates_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("usage_aggregates", { schema: "zeroship" }).foreignKey("usage_aggregates_metric_fkey").add({ columns: ["metric"], references: { table: "billing_metrics", columns: ["metric"], schema: "zeroship" }, deferrable: true, initiallyDeferred: true });

    // ---- typed-id collations that the RLS file would otherwise strand -------
    //
    // These eight `app_id` columns are the ones named in a `tenant_isolation`
    // USING/WITH CHECK expression, and PostgreSQL answers `cannot alter type of
    // a column used in a policy definition` for every one of them. They must
    // therefore be collated HERE, after the tables exist and before
    // 20260702000800_policies_rls creates the policies - not in
    // 20260831000001_sortable_entity_id_collations with the rest of the domain.
    //
    // Only the policy-referenced column moves. Each of these tables keeps its
    // OTHER typed-id columns in the later map, because a single `ALTER TABLE`
    // carrying several `ALTER COLUMN` clauses fails as a whole if any one clause
    // names a policy column - so the split is per column, not per table.
    const policyPinnedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
      app_secrets: ["app_id"],
      app_session_anchors: ["app_id"],
      app_spend_limit: ["app_id"],
      app_spend_state: ["app_id"],
      gateway_sessions: ["app_id"],
      plan_change_events: ["app_id"],
      spend_state_history: ["app_id"],
      usage_aggregates: ["app_id"],
    };
    for (const [tableName, columns] of Object.entries(policyPinnedIdColumnsByTable)) {
      const alterations = columns
        .map((column) => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`)
        .join(", ");
      raw({
        sql: `ALTER TABLE "zeroship"."${tableName}" ${alterations}`,
        reason:
          "typed-id text domains need bytewise comparison, and a policy on the column makes this "
          + "the last point the type can be altered at all",
      });
    }
  },
};
