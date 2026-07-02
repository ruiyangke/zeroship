import { raw } from "@zeroship/migrate/pg";

export const name = "constraints_indexes_fks";

export function up() {
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_audit\n    ADD CONSTRAINT app_audit_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_env_expose\n    ADD CONSTRAINT app_env_expose_pkey PRIMARY KEY (app_id, key_name)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_members\n    ADD CONSTRAINT app_members_pkey PRIMARY KEY (app_id, user_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_net_grants\n    ADD CONSTRAINT app_net_grants_pkey PRIMARY KEY (app_id, host, port)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_oauth_clients\n    ADD CONSTRAINT app_oauth_clients_pkey PRIMARY KEY (app_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_scope_defs\n    ADD CONSTRAINT app_scope_defs_pkey PRIMARY KEY (app_id, scope_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_secrets\n    ADD CONSTRAINT app_secrets_pkey PRIMARY KEY (app_id, key_name)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors\n    ADD CONSTRAINT app_session_anchors_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_limit\n    ADD CONSTRAINT app_spend_limit_pkey PRIMARY KEY (app_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state\n    ADD CONSTRAINT app_spend_state_pkey PRIMARY KEY (app_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_usage\n    ADD CONSTRAINT app_usage_pkey PRIMARY KEY (app_id, resource)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_user_identities\n    ADD CONSTRAINT app_user_identities_pkey PRIMARY KEY (app_client_id, global_user_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.app_vars\n    ADD CONSTRAINT app_vars_pkey PRIMARY KEY (app_id, key_name)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.apps\n    ADD CONSTRAINT apps_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.audit_events\n    ADD CONSTRAINT audit_events_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.authz_decisions\n    ADD CONSTRAINT authz_decisions_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_customer_refs\n    ADD CONSTRAINT billing_customer_refs_pkey PRIMARY KEY (creator_id, provider)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes\n    ADD CONSTRAINT billing_disputes_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_line_provider_refs\n    ADD CONSTRAINT billing_line_provider_refs_pkey PRIMARY KEY (invoice_id, app_id, segment_no, provider, ref_kind)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_metrics\n    ADD CONSTRAINT billing_metrics_pkey PRIMARY KEY (metric)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_notifications\n    ADD CONSTRAINT billing_notifications_pkey PRIMARY KEY (creator_id, kind, transition_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_provider_refs\n    ADD CONSTRAINT billing_provider_refs_pkey PRIMARY KEY (invoice_id, provider, ref_kind)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_reconciliation_findings\n    ADD CONSTRAINT billing_reconciliation_findings_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures\n    ADD CONSTRAINT connect_checkout_failures_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_account_history\n    ADD CONSTRAINT creator_account_history_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_accounts\n    ADD CONSTRAINT creator_accounts_pkey PRIMARY KEY (creator_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing\n    ADD CONSTRAINT creator_billing_pkey PRIMARY KEY (creator_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status_history\n    ADD CONSTRAINT creator_billing_status_history_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status\n    ADD CONSTRAINT creator_billing_status_pkey PRIMARY KEY (creator_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy\n    ADD CONSTRAINT creator_fee_policy_pkey PRIMARY KEY (creator_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger\n    ADD CONSTRAINT credit_ledger_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.cron_state\n    ADD CONSTRAINT cron_state_pkey PRIMARY KEY (key)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.deleted_sandboxes\n    ADD CONSTRAINT deleted_sandboxes_pkey PRIMARY KEY (sandbox_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants\n    ADD CONSTRAINT device_grants_pkey PRIMARY KEY (device_code_hash)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.dpop_jti\n    ADD CONSTRAINT dpop_jti_pkey PRIMARY KEY (jti)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.email_suppressions\n    ADD CONSTRAINT email_suppressions_pkey PRIMARY KEY (email)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.email_verifications\n    ADD CONSTRAINT email_verifications_pkey PRIMARY KEY (token_hash)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.federated_identities\n    ADD CONSTRAINT federated_identities_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions\n    ADD CONSTRAINT gateway_sessions_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts\n    ADD CONSTRAINT hosts_pkey PRIMARY KEY (host_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.identity_links\n    ADD CONSTRAINT identity_links_pkey PRIMARY KEY (provider, provider_subject)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.idp_sessions\n    ADD CONSTRAINT idp_sessions_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines\n    ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id, segment_no)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments\n    ADD CONSTRAINT invoice_payments_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices\n    ADD CONSTRAINT invoices_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.jwk_key_state\n    ADD CONSTRAINT jwk_key_state_pkey PRIMARY KEY (set_name, kid)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_completions\n    ADD CONSTRAINT magic_completions_pkey PRIMARY KEY (csrf_nonce)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_links\n    ADD CONSTRAINT magic_links_pkey PRIMARY KEY (token_hash)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports\n    ADD CONSTRAINT metering_exports_pkey PRIMARY KEY (creator_id, period)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.metric_weights\n    ADD CONSTRAINT metric_weights_pkey PRIMARY KEY (metric)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_app_policies\n    ADD CONSTRAINT migrated_app_policies_pkey PRIMARY KEY (app_id, version)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit\n    ADD CONSTRAINT migrated_migration_audit_pkey PRIMARY KEY (audit_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations\n    ADD CONSTRAINT migrated_migrations_pkey PRIMARY KEY (app_id, migration_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.net_policy_catalog\n    ADD CONSTRAINT net_policy_catalog_pkey PRIMARY KEY (key)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes\n    ADD CONSTRAINT oauth_authorization_codes_pkey PRIMARY KEY (code_hash)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients\n    ADD CONSTRAINT oauth_clients_pkey PRIMARY KEY (client_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_grants\n    ADD CONSTRAINT oauth_grants_pkey PRIMARY KEY (user_id, client_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens\n    ADD CONSTRAINT oauth_refresh_tokens_pkey PRIMARY KEY (token_hash)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.oidc_session_clients\n    ADD CONSTRAINT oidc_session_clients_pkey PRIMARY KEY (idp_session_id, client_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures\n    ADD CONSTRAINT payout_failures_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts\n    ADD CONSTRAINT payouts_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes\n    ADD CONSTRAINT pending_disputes_pkey PRIMARY KEY (provider_dispute_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.permission_tokens\n    ADD CONSTRAINT permission_tokens_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events\n    ADD CONSTRAINT plan_change_events_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.plans\n    ADD CONSTRAINT plans_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_admin_roles\n    ADD CONSTRAINT platform_admin_roles_pkey PRIMARY KEY (user_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_policies\n    ADD CONSTRAINT platform_policies_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.pricing_config\n    ADD CONSTRAINT pricing_config_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.principal_grants\n    ADD CONSTRAINT principal_grants_pkey PRIMARY KEY (principal_id, grant_name)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.rate_limits\n    ADD CONSTRAINT rate_limits_pkey PRIMARY KEY (bucket_key)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.refund_provider_refs\n    ADD CONSTRAINT refund_provider_refs_pkey PRIMARY KEY (refund_id, provider, ref_kind)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds\n    ADD CONSTRAINT refunds_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events\n    ADD CONSTRAINT sandbox_events_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05\n    ADD CONSTRAINT sandbox_events_2026_05_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06\n    ADD CONSTRAINT sandbox_events_2026_06_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07\n    ADD CONSTRAINT sandbox_events_2026_07_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08\n    ADD CONSTRAINT sandbox_events_2026_08_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09\n    ADD CONSTRAINT sandbox_events_2026_09_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10\n    ADD CONSTRAINT sandbox_events_2026_10_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default\n    ADD CONSTRAINT sandbox_events_default_pkey PRIMARY KEY (ts, event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes\n    ADD CONSTRAINT sandboxes_pkey PRIMARY KEY (sandbox_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.shares\n    ADD CONSTRAINT shares_pkey PRIMARY KEY (token_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.signing_keys\n    ADD CONSTRAINT signing_keys_pkey PRIMARY KEY (kid)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history\n    ADD CONSTRAINT spend_state_history_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.stripe_events_seen\n    ADD CONSTRAINT stripe_events_seen_pkey PRIMARY KEY (event_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.token_revocations\n    ADD CONSTRAINT token_revocations_pkey PRIMARY KEY (client_id, sub)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.totp_backup_codes\n    ADD CONSTRAINT totp_backup_codes_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.totp_credentials\n    ADD CONSTRAINT totp_credentials_pkey PRIMARY KEY (user_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates\n    ADD CONSTRAINT usage_aggregates_pkey PRIMARY KEY (app_id, period, metric)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_reports_seen\n    ADD CONSTRAINT usage_reports_seen_pkey PRIMARY KEY (worker_id, sequence)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.users\n    ADD CONSTRAINT users_pkey PRIMARY KEY (id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys
  raw({ sql: "ALTER TABLE ONLY zeroship.wake_jobs\n    ADD CONSTRAINT wake_jobs_pkey PRIMARY KEY (wake_id)", reason: "the table handle has no standalone primary-key constraint operation for existing exact platform tables" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_oauth_clients\n    ADD CONSTRAINT app_oauth_clients_client_id_key UNIQUE (client_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_usage_history\n    ADD CONSTRAINT app_usage_history_app_id_period_key UNIQUE (app_id, period)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.apps\n    ADD CONSTRAINT apps_name_key UNIQUE (name)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_customer_refs\n    ADD CONSTRAINT billing_customer_refs_external_id_key UNIQUE (external_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes\n    ADD CONSTRAINT billing_disputes_provider_dispute_id_key UNIQUE (provider_dispute_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_line_provider_refs\n    ADD CONSTRAINT billing_line_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_provider_refs\n    ADD CONSTRAINT billing_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_reconciliation_findings\n    ADD CONSTRAINT billing_reconciliation_findings_dedup_key_key UNIQUE (dedup_key)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures\n    ADD CONSTRAINT connect_checkout_failures_provider_payment_intent_id_key UNIQUE (provider_payment_intent_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants\n    ADD CONSTRAINT device_grants_user_code_key UNIQUE (user_code)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.federated_identities\n    ADD CONSTRAINT federated_identities_provider_subject_key UNIQUE (provider, subject)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures\n    ADD CONSTRAINT payout_failures_provider_payout_id_key UNIQUE (provider_payout_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts\n    ADD CONSTRAINT payouts_event_id_key UNIQUE (event_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.refund_provider_refs\n    ADD CONSTRAINT refund_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds\n    ADD CONSTRAINT refunds_idempotency_key_key UNIQUE (idempotency_key)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.users\n    ADD CONSTRAINT users_email_key UNIQUE (email)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX app_members_user_idx ON zeroship.app_members USING btree (user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX app_net_grants_app_id_idx ON zeroship.app_net_grants USING btree (app_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX app_session_anchors_user_idx ON zeroship.app_session_anchors USING btree (app_id, global_user_id) WHERE (revoked_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX app_user_identities_pairwise_sub_idx ON zeroship.app_user_identities USING btree (pairwise_sub)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX app_user_identities_relay_active_idx ON zeroship.app_user_identities USING btree (relay_email) WHERE ((relay_email IS NOT NULL) AND (revoked_at IS NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX apps_plan_id_idx ON zeroship.apps USING btree (plan_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_audit_event_idx ON zeroship.audit_events USING btree (event_type, occurred_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_audit_user_idx ON zeroship.audit_events USING btree (actor_user_id, occurred_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_dpop_jti_inserted_idx ON zeroship.dpop_jti USING btree (inserted_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_gateway_sessions_app_idx ON zeroship.gateway_sessions USING btree (app_id, user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_gateway_sessions_app_sid_idx ON zeroship.gateway_sessions USING btree (app_id, sid) WHERE ((sid IS NOT NULL) AND (revoked_at IS NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_gateway_sessions_idle_idx ON zeroship.gateway_sessions USING btree (idle_expires_at) WHERE (revoked_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_magic_completions_expires_idx ON zeroship.magic_completions USING btree (expires_at) WHERE (consumed_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_magic_email_idx ON zeroship.magic_links USING btree (email)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_magic_user_id_idx ON zeroship.magic_links USING btree (user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_rate_limits_updated_at_idx ON zeroship.rate_limits USING btree (updated_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_token_revocations_revoked_after_idx ON zeroship.token_revocations USING btree (revoked_after)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_totp_backup_codes_user_idx ON zeroship.totp_backup_codes USING btree (user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX auth_users_deletion_due_idx ON zeroship.users USING btree (deletion_scheduled_for) WHERE ((deletion_scheduled_for IS NOT NULL) AND (anonymized_at IS NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX authz_decisions_occurred_idx ON zeroship.authz_decisions USING btree (occurred_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX authz_decisions_user_idx ON zeroship.authz_decisions USING btree (actor_user_id) WHERE (actor_user_id IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX billing_disputes_invoice_idx ON zeroship.billing_disputes USING btree (invoice_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX billing_metrics_owner_app_idx ON zeroship.billing_metrics USING btree (owner_app) WHERE (owner_app IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX billing_reconciliation_findings_kind_idx ON zeroship.billing_reconciliation_findings USING btree (kind, detected_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX billing_reconciliation_findings_open_idx ON zeroship.billing_reconciliation_findings USING btree (detected_at) WHERE (resolved_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX connect_checkout_failures_creator_idx ON zeroship.connect_checkout_failures USING btree (creator_id, created_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX credit_ledger_creator_created_idx ON zeroship.credit_ledger USING btree (creator_id, created_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_idempotency_key_idx ON zeroship.credit_ledger USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_refund_clawback_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_clawback'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_refund_to_credit_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_to_credit'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX device_grants_client_id_idx ON zeroship.device_grants USING btree (client_id) WHERE (client_id IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX device_grants_expires_at_idx ON zeroship.device_grants USING btree (expires_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX device_grants_provider_pending_user_code_idx ON zeroship.device_grants USING btree (provider, user_code) WHERE (status = 'pending'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX identity_links_principal_id_idx ON zeroship.identity_links USING btree (principal_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_app_audit_app_at ON zeroship.app_audit USING btree (app_id, occurred_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_app_audit_creator_at ON zeroship.app_audit USING btree (creator_id, occurred_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_billing_notifications_pending ON zeroship.billing_notifications USING btree (claimed_at) WHERE ((status)::text = 'pending'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_creator_account_history_creator ON zeroship.creator_account_history USING btree (creator_id, linked_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX idx_creator_account_history_one_open ON zeroship.creator_account_history USING btree (creator_id) WHERE (unlinked_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_creator_billing_status_history_creator_at ON zeroship.creator_billing_status_history USING btree (creator_id, at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_creator_billing_status_past_due ON zeroship.creator_billing_status USING btree (past_due_since) WHERE ((state)::text = 'past_due'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_deleted_sandboxes_deleted_at ON zeroship.deleted_sandboxes USING btree (deleted_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_hosts_region_status ON zeroship.hosts USING btree (region, status)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_hosts_status_heartbeat ON zeroship.hosts USING btree (status, last_heartbeat)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_payouts_creator_time ON zeroship.payouts USING btree (creator_id, occurred_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandbox_events_metering ON ONLY zeroship.sandbox_events USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandbox_events_sandbox_ts ON ONLY zeroship.sandbox_events USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandbox_events_ts_brin ON ONLY zeroship.sandbox_events USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandbox_events_user_id_ts ON ONLY zeroship.sandbox_events USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX idx_sandboxes_active_user_project ON zeroship.sandboxes USING btree (user_id, project_id) WHERE ((deleted_at IS NULL) AND (status = ANY (ARRAY['starting'::text, 'running'::text, 'recreating'::text])))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandboxes_created_at ON zeroship.sandboxes USING btree (created_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandboxes_host_id_status ON zeroship.sandboxes USING btree (host_id, status) WHERE (deleted_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandboxes_status_last_used ON zeroship.sandboxes USING btree (status, last_used_at) WHERE (deleted_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_sandboxes_user_id ON zeroship.sandboxes USING btree (user_id) WHERE (deleted_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_shares_expires_at ON zeroship.shares USING btree (expires_at) WHERE ((deleted_at IS NULL) AND (revoked_at IS NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_shares_iss_issued_at ON zeroship.shares USING btree (iss, issued_at) WHERE ((deleted_at IS NULL) AND (iss IS NOT NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_shares_sandbox_id_port ON zeroship.shares USING btree (sandbox_id, port) WHERE (deleted_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history USING btree (app_id, at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX idx_spend_state_history_period ON zeroship.spend_state_history USING btree (period)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX invoice_payments_charge_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref) WHERE ((kind)::text = 'charge'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX invoice_payments_dispute_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref, kind) WHERE ((kind)::text = ANY (ARRAY['dispute_debit'::text, 'dispute_reversal'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX invoice_payments_invoice_idx ON zeroship.invoice_payments USING btree (invoice_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX invoices_active_period_claim ON zeroship.invoices USING btree (creator_id, period) WHERE ((status)::text <> 'void'::text)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX migrated_app_policies_app_submitted_idx ON zeroship.migrated_app_policies USING btree (app_id, submitted_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX migrated_migration_audit_app_idx ON zeroship.migrated_migration_audit USING btree (app_id, migration_id, created_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX migrated_migrations_app_status_idx ON zeroship.migrated_migrations USING btree (app_id, status, submitted_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_authorization_codes_expires_at_idx ON zeroship.oauth_authorization_codes USING btree (expires_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_grants_client_idx ON zeroship.oauth_grants USING btree (client_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_grants_user_granted_idx ON zeroship.oauth_grants USING btree (user_id, granted_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_refresh_tokens_expires_at_idx ON zeroship.oauth_refresh_tokens USING btree (expires_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_refresh_tokens_family_idx ON zeroship.oauth_refresh_tokens USING btree (refresh_family_id, client_id, user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_refresh_tokens_idem_reap_idx ON zeroship.oauth_refresh_tokens USING btree (idem_expires_at) WHERE (idem_response_enc IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX oauth_refresh_tokens_one_active_per_family ON zeroship.oauth_refresh_tokens USING btree (refresh_family_id) WHERE ((rotated_at IS NULL) AND (revoked_at IS NULL))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oauth_refresh_tokens_user_idx ON zeroship.oauth_refresh_tokens USING btree (user_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oidc_session_clients_client_idx ON zeroship.oidc_session_clients USING btree (client_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX oidc_session_clients_user_idx ON zeroship.oidc_session_clients USING btree (user_id, idp_session_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX payout_failures_creator_idx ON zeroship.payout_failures USING btree (creator_id, created_at DESC)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX pending_disputes_charge_idx ON zeroship.pending_disputes USING btree (charge) WHERE (charge IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX pending_disputes_payment_intent_idx ON zeroship.pending_disputes USING btree (payment_intent) WHERE (payment_intent IS NOT NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX permission_tokens_owner_active_idx ON zeroship.permission_tokens USING btree (owner_id) WHERE (revoked_at IS NULL)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX permission_tokens_policies_gin_idx ON zeroship.permission_tokens USING gin (policies)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX plan_change_events_app_period_idx ON zeroship.plan_change_events USING btree (app_id, period)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX refunds_invoice_idx ON zeroship.refunds USING btree (invoice_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_05 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_ts_idx ON zeroship.sandbox_events_2026_05 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_05 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_user_id_ts_idx ON zeroship.sandbox_events_2026_05 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_06 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_ts_idx ON zeroship.sandbox_events_2026_06 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_06 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_user_id_ts_idx ON zeroship.sandbox_events_2026_06 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_07 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_ts_idx ON zeroship.sandbox_events_2026_07 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_07 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_user_id_ts_idx ON zeroship.sandbox_events_2026_07 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_08 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_ts_idx ON zeroship.sandbox_events_2026_08 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_08 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_user_id_ts_idx ON zeroship.sandbox_events_2026_08 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_09 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_ts_idx ON zeroship.sandbox_events_2026_09 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_09 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_user_id_ts_idx ON zeroship.sandbox_events_2026_09 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_10 USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_ts_idx ON zeroship.sandbox_events_2026_10 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_10 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_user_id_ts_idx ON zeroship.sandbox_events_2026_10 USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_default_sandbox_id_ts_idx ON zeroship.sandbox_events_default USING btree (sandbox_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_default_ts_idx ON zeroship.sandbox_events_default USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_default_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_default USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandbox_events_default_user_id_ts_idx ON zeroship.sandbox_events_default USING btree (user_id, ts)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandboxes_idle_snapshot_idx ON zeroship.sandboxes USING btree (last_used_at) WHERE ((status = 'running'::text) AND idle_snapshot_opted_in)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX sandboxes_status_lessee_idx ON zeroship.sandboxes USING btree (status, lessee_updated_at) WHERE (status = ANY (ARRAY['snapshotting'::text, 'restoring'::text, 'restoring_cold'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX signing_keys_status_idx ON zeroship.signing_keys USING btree (status)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX usage_aggregates_period_idx ON zeroship.usage_aggregates USING btree (period, app_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX wake_jobs_lessee_idx ON zeroship.wake_jobs USING btree (lessee_updated_at) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX wake_jobs_sandbox_idx ON zeroship.wake_jobs USING btree (sandbox_id)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE UNIQUE INDEX wake_jobs_sandbox_pending_uniq ON zeroship.wake_jobs USING btree (sandbox_id) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX wake_jobs_state_idx ON zeroship.wake_jobs USING btree (state) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed
  raw({ sql: "CREATE INDEX wake_jobs_updated_at_idx ON zeroship.wake_jobs USING btree (updated_at)", reason: "this index uses options the current index DSL cannot render exactly" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_env_expose\n    ADD CONSTRAINT app_env_expose_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_members\n    ADD CONSTRAINT app_members_added_by_fkey FOREIGN KEY (added_by) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_members\n    ADD CONSTRAINT app_members_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_members\n    ADD CONSTRAINT app_members_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_net_grants\n    ADD CONSTRAINT app_net_grants_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_oauth_clients\n    ADD CONSTRAINT app_oauth_clients_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_oauth_clients\n    ADD CONSTRAINT app_oauth_clients_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_scope_defs\n    ADD CONSTRAINT app_scope_defs_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_secrets\n    ADD CONSTRAINT app_secrets_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors\n    ADD CONSTRAINT app_session_anchors_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors\n    ADD CONSTRAINT app_session_anchors_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors\n    ADD CONSTRAINT app_session_anchors_global_user_id_fkey FOREIGN KEY (global_user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_limit\n    ADD CONSTRAINT app_spend_limit_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state\n    ADD CONSTRAINT app_spend_state_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_usage\n    ADD CONSTRAINT app_usage_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_usage_history\n    ADD CONSTRAINT app_usage_history_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_user_identities\n    ADD CONSTRAINT app_user_identities_app_client_id_fkey FOREIGN KEY (app_client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_user_identities\n    ADD CONSTRAINT app_user_identities_global_user_id_fkey FOREIGN KEY (global_user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.app_vars\n    ADD CONSTRAINT app_vars_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.apps\n    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_customer_refs\n    ADD CONSTRAINT billing_customer_refs_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes\n    ADD CONSTRAINT billing_disputes_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_line_provider_refs\n    ADD CONSTRAINT billing_line_provider_refs_line_fk FOREIGN KEY (invoice_id, app_id, segment_no) REFERENCES zeroship.invoice_lines(invoice_id, app_id, segment_no) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_metrics\n    ADD CONSTRAINT billing_metrics_owner_app_fkey FOREIGN KEY (owner_app) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_notifications\n    ADD CONSTRAINT billing_notifications_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_provider_refs\n    ADD CONSTRAINT billing_provider_refs_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures\n    ADD CONSTRAINT connect_checkout_failures_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_accounts\n    ADD CONSTRAINT creator_accounts_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing\n    ADD CONSTRAINT creator_billing_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status\n    ADD CONSTRAINT creator_billing_status_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status_history\n    ADD CONSTRAINT creator_billing_status_history_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy\n    ADD CONSTRAINT creator_fee_policy_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger\n    ADD CONSTRAINT credit_ledger_applied_invoice_id_fkey FOREIGN KEY (applied_invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger\n    ADD CONSTRAINT credit_ledger_consumed_from_grant_id_fkey FOREIGN KEY (consumed_from_grant_id) REFERENCES zeroship.credit_ledger(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger\n    ADD CONSTRAINT credit_ledger_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants\n    ADD CONSTRAINT device_grants_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants\n    ADD CONSTRAINT device_grants_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.email_verifications\n    ADD CONSTRAINT email_verifications_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.federated_identities\n    ADD CONSTRAINT federated_identities_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions\n    ADD CONSTRAINT gateway_sessions_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.identity_links\n    ADD CONSTRAINT identity_links_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.idp_sessions\n    ADD CONSTRAINT idp_sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines\n    ADD CONSTRAINT invoice_lines_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines\n    ADD CONSTRAINT invoice_lines_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines\n    ADD CONSTRAINT invoice_lines_plan_id_fkey FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments\n    ADD CONSTRAINT invoice_payments_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices\n    ADD CONSTRAINT invoices_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_links\n    ADD CONSTRAINT magic_links_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports\n    ADD CONSTRAINT metering_exports_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.metric_weights\n    ADD CONSTRAINT metric_weights_metric_fkey FOREIGN KEY (metric) REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_app_policies\n    ADD CONSTRAINT migrated_app_policies_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_app_policies\n    ADD CONSTRAINT migrated_app_policies_submitted_by_fkey FOREIGN KEY (submitted_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit\n    ADD CONSTRAINT migrated_migration_audit_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit\n    ADD CONSTRAINT migrated_migration_audit_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations\n    ADD CONSTRAINT migrated_migrations_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations\n    ADD CONSTRAINT migrated_migrations_approved_by_fkey FOREIGN KEY (approved_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations\n    ADD CONSTRAINT migrated_migrations_submitted_by_fkey FOREIGN KEY (submitted_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes\n    ADD CONSTRAINT oauth_authorization_codes_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes\n    ADD CONSTRAINT oauth_authorization_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients\n    ADD CONSTRAINT oauth_clients_created_by_fkey FOREIGN KEY (created_by) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_grants\n    ADD CONSTRAINT oauth_grants_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_grants\n    ADD CONSTRAINT oauth_grants_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens\n    ADD CONSTRAINT oauth_refresh_tokens_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens\n    ADD CONSTRAINT oauth_refresh_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oidc_session_clients\n    ADD CONSTRAINT oidc_session_clients_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oidc_session_clients\n    ADD CONSTRAINT oidc_session_clients_idp_session_id_fkey FOREIGN KEY (idp_session_id) REFERENCES zeroship.idp_sessions(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.oidc_session_clients\n    ADD CONSTRAINT oidc_session_clients_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures\n    ADD CONSTRAINT payout_failures_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts\n    ADD CONSTRAINT payouts_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.permission_tokens\n    ADD CONSTRAINT permission_tokens_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events\n    ADD CONSTRAINT plan_change_events_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events\n    ADD CONSTRAINT plan_change_events_from_plan_id_fkey FOREIGN KEY (from_plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events\n    ADD CONSTRAINT plan_change_events_to_plan_id_fkey FOREIGN KEY (to_plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_admin_roles\n    ADD CONSTRAINT platform_admin_roles_granted_by_fkey FOREIGN KEY (granted_by) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_admin_roles\n    ADD CONSTRAINT platform_admin_roles_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_policies\n    ADD CONSTRAINT platform_policies_updated_by_fkey FOREIGN KEY (updated_by) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.principal_grants\n    ADD CONSTRAINT principal_grants_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.refund_provider_refs\n    ADD CONSTRAINT refund_provider_refs_refund_id_fkey FOREIGN KEY (refund_id) REFERENCES zeroship.refunds(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds\n    ADD CONSTRAINT refunds_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes\n    ADD CONSTRAINT sandboxes_host_id_fkey FOREIGN KEY (host_id) REFERENCES zeroship.hosts(host_id) ON DELETE RESTRICT", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.shares\n    ADD CONSTRAINT shares_sandbox_id_fkey FOREIGN KEY (sandbox_id) REFERENCES zeroship.sandboxes(sandbox_id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history\n    ADD CONSTRAINT spend_state_history_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.totp_backup_codes\n    ADD CONSTRAINT totp_backup_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.totp_credentials\n    ADD CONSTRAINT totp_credentials_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates\n    ADD CONSTRAINT usage_aggregates_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add structural support for this table constraint shape
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates\n    ADD CONSTRAINT usage_aggregates_metric_fkey FOREIGN KEY (metric) REFERENCES zeroship.billing_metrics(metric) DEFERRABLE INITIALLY DEFERRED", reason: "this constraint shape is outside the current structural renderer" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_05_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_05_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_05_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_05_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_05_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_06_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_06_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_06_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_06_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_06_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_07_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_07_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_07_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_07_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_07_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_08_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_08_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_08_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_08_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_08_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_09_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_09_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_09_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_09_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_09_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_10_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_10_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_10_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_10_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_10_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_default_pkey", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_default_sandbox_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_default_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_default_ts_sandbox_id_user_id_data_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_default_user_id_ts_idx", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
}

export function down() {

}
