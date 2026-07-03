import { notMembership, table } from "@zeroship/migrate";
import { raw } from "@zeroship/migrate/pg";

export const name = "constraints_indexes_fks";

export function up() {

  // TODO(dsl-v2): primary key stays raw until partitioned table createTable support registers sandbox_events structurally
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events\n    ADD CONSTRAINT sandbox_events_pkey PRIMARY KEY (ts, event_id)", reason: "sandbox_events remains raw because createTable cannot express PARTITION BY RANGE yet" });

  table("app_oauth_clients", { schema: "zeroship" }).unique("app_oauth_clients_client_id_key").add({ columns: ["client_id"] });
  table("app_usage_history", { schema: "zeroship" }).unique("app_usage_history_app_id_period_key").add({ columns: ["app_id", "period"] });
  table("apps", { schema: "zeroship" }).unique("apps_name_key").add({ columns: ["name"] });
  table("billing_customer_refs", { schema: "zeroship" }).unique("billing_customer_refs_external_id_key").add({ columns: ["external_id"] });
  table("billing_disputes", { schema: "zeroship" }).unique("billing_disputes_provider_dispute_id_key").add({ columns: ["provider_dispute_id"] });
  table("billing_line_provider_refs", { schema: "zeroship" }).unique("billing_line_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
  table("billing_provider_refs", { schema: "zeroship" }).unique("billing_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
  table("billing_reconciliation_findings", { schema: "zeroship" }).unique("billing_reconciliation_findings_dedup_key_key").add({ columns: ["dedup_key"] });
  table("connect_checkout_failures", { schema: "zeroship" }).unique("connect_checkout_failures_provider_payment_intent_id_key").add({ columns: ["provider_payment_intent_id"] });
  table("device_grants", { schema: "zeroship" }).unique("device_grants_user_code_key").add({ columns: ["user_code"] });
  table("federated_identities", { schema: "zeroship" }).unique("federated_identities_provider_subject_key").add({ columns: ["provider", "subject"] });
  table("payout_failures", { schema: "zeroship" }).unique("payout_failures_provider_payout_id_key").add({ columns: ["provider_payout_id"] });
  table("payouts", { schema: "zeroship" }).unique("payouts_event_id_key").add({ columns: ["event_id"] });
  table("refund_provider_refs", { schema: "zeroship" }).unique("refund_provider_refs_provider_ref_kind_external_id_key").add({ columns: ["provider", "ref_kind", "external_id"] });
  table("refunds", { schema: "zeroship" }).unique("refunds_idempotency_key_key").add({ columns: ["idempotency_key"] });
  table("users", { schema: "zeroship" }).unique("users_email_key").add({ columns: ["email"] });
  table("app_members", { schema: "zeroship" }).index("app_members_user_idx").add({ columns: ["user_id"] });
  table("app_net_grants", { schema: "zeroship" }).index("app_net_grants_app_id_idx").add({ columns: ["app_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX app_session_anchors_user_idx ON zeroship.app_session_anchors USING btree (app_id, global_user_id) WHERE (revoked_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("app_user_identities", { schema: "zeroship" }).index("app_user_identities_pairwise_sub_idx").add({ columns: ["pairwise_sub"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX app_user_identities_relay_active_idx ON zeroship.app_user_identities USING btree (relay_email) WHERE ((relay_email IS NOT NULL) AND (revoked_at IS NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("apps", { schema: "zeroship" }).index("apps_plan_id_idx").add({ columns: ["plan_id"] });
  table("audit_events", { schema: "zeroship" }).index("auth_audit_event_idx").add({ columns: ["event_type", "occurred_at"] });
  table("audit_events", { schema: "zeroship" }).index("auth_audit_user_idx").add({ columns: ["actor_user_id", "occurred_at"] });
  table("dpop_jti", { schema: "zeroship" }).index("auth_dpop_jti_inserted_idx").add({ columns: ["inserted_at"] });
  table("gateway_sessions", { schema: "zeroship" }).index("auth_gateway_sessions_app_idx").add({ columns: ["app_id", "user_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX auth_gateway_sessions_app_sid_idx ON zeroship.gateway_sessions USING btree (app_id, sid) WHERE ((sid IS NOT NULL) AND (revoked_at IS NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX auth_gateway_sessions_idle_idx ON zeroship.gateway_sessions USING btree (idle_expires_at) WHERE (revoked_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX auth_magic_completions_expires_idx ON zeroship.magic_completions USING btree (expires_at) WHERE (consumed_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("magic_links", { schema: "zeroship" }).index("auth_magic_email_idx").add({ columns: ["email"] });
  table("magic_links", { schema: "zeroship" }).index("auth_magic_user_id_idx").add({ columns: ["user_id"] });
  table("rate_limits", { schema: "zeroship" }).index("auth_rate_limits_updated_at_idx").add({ columns: ["updated_at"] });
  table("token_revocations", { schema: "zeroship" }).index("auth_token_revocations_revoked_after_idx").add({ columns: ["revoked_after"] });
  table("totp_backup_codes", { schema: "zeroship" }).index("auth_totp_backup_codes_user_idx").add({ columns: ["user_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX auth_users_deletion_due_idx ON zeroship.users USING btree (deletion_scheduled_for) WHERE ((deletion_scheduled_for IS NOT NULL) AND (anonymized_at IS NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX authz_decisions_occurred_idx ON zeroship.authz_decisions USING btree (occurred_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX authz_decisions_user_idx ON zeroship.authz_decisions USING btree (actor_user_id) WHERE (actor_user_id IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("billing_disputes", { schema: "zeroship" }).index("billing_disputes_invoice_idx").add({ columns: ["invoice_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX billing_metrics_owner_app_idx ON zeroship.billing_metrics USING btree (owner_app) WHERE (owner_app IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("billing_reconciliation_findings", { schema: "zeroship" }).index("billing_reconciliation_findings_kind_idx").add({ columns: ["kind", "detected_at"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX billing_reconciliation_findings_open_idx ON zeroship.billing_reconciliation_findings USING btree (detected_at) WHERE (resolved_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX connect_checkout_failures_creator_idx ON zeroship.connect_checkout_failures USING btree (creator_id, created_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("credit_ledger", { schema: "zeroship" }).index("credit_ledger_creator_created_idx").add({ columns: ["creator_id", "created_at"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_idempotency_key_idx ON zeroship.credit_ledger USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_refund_clawback_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_clawback'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX credit_ledger_refund_to_credit_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_to_credit'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX device_grants_client_id_idx ON zeroship.device_grants USING btree (client_id) WHERE (client_id IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("device_grants", { schema: "zeroship" }).index("device_grants_expires_at_idx").add({ columns: ["expires_at"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX device_grants_provider_pending_user_code_idx ON zeroship.device_grants USING btree (provider, user_code) WHERE (status = 'pending'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("identity_links", { schema: "zeroship" }).index("identity_links_principal_id_idx").add({ columns: ["principal_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_app_audit_app_at ON zeroship.app_audit USING btree (app_id, occurred_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_app_audit_creator_at ON zeroship.app_audit USING btree (creator_id, occurred_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_billing_notifications_pending ON zeroship.billing_notifications USING btree (claimed_at) WHERE ((status)::text = 'pending'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_creator_account_history_creator ON zeroship.creator_account_history USING btree (creator_id, linked_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX idx_creator_account_history_one_open ON zeroship.creator_account_history USING btree (creator_id) WHERE (unlinked_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_creator_billing_status_history_creator_at ON zeroship.creator_billing_status_history USING btree (creator_id, at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_creator_billing_status_past_due ON zeroship.creator_billing_status USING btree (past_due_since) WHERE ((state)::text = 'past_due'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("deleted_sandboxes", { schema: "zeroship" }).index("idx_deleted_sandboxes_deleted_at").add({ columns: ["deleted_at"] });
  table("hosts", { schema: "zeroship" }).index("idx_hosts_region_status").add({ columns: ["region", "status"] });
  table("hosts", { schema: "zeroship" }).index("idx_hosts_status_heartbeat").add({ columns: ["status", "last_heartbeat"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_payouts_creator_time ON zeroship.payouts USING btree (creator_id, occurred_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandbox_events_metering ON ONLY zeroship.sandbox_events USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandbox_events_sandbox_ts ON ONLY zeroship.sandbox_events USING btree (sandbox_id, ts)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandbox_events_ts_brin ON ONLY zeroship.sandbox_events USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandbox_events_user_id_ts ON ONLY zeroship.sandbox_events USING btree (user_id, ts)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX idx_sandboxes_active_user_project ON zeroship.sandboxes USING btree (user_id, project_id) WHERE ((deleted_at IS NULL) AND (status = ANY (ARRAY['starting'::text, 'running'::text, 'recreating'::text])))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandboxes", { schema: "zeroship" }).index("idx_sandboxes_created_at").add({ columns: ["created_at"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandboxes_host_id_status ON zeroship.sandboxes USING btree (host_id, status) WHERE (deleted_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandboxes_status_last_used ON zeroship.sandboxes USING btree (status, last_used_at) WHERE (deleted_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_sandboxes_user_id ON zeroship.sandboxes USING btree (user_id) WHERE (deleted_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_shares_expires_at ON zeroship.shares USING btree (expires_at) WHERE ((deleted_at IS NULL) AND (revoked_at IS NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_shares_iss_issued_at ON zeroship.shares USING btree (iss, issued_at) WHERE ((deleted_at IS NULL) AND (iss IS NOT NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_shares_sandbox_id_port ON zeroship.shares USING btree (sandbox_id, port) WHERE (deleted_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history USING btree (app_id, at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("spend_state_history", { schema: "zeroship" }).index("idx_spend_state_history_period").add({ columns: ["period"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX invoice_payments_charge_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref) WHERE ((kind)::text = 'charge'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX invoice_payments_dispute_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref, kind) WHERE ((kind)::text = ANY (ARRAY['dispute_debit'::text, 'dispute_reversal'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("invoice_payments", { schema: "zeroship" }).index("invoice_payments_invoice_idx").add({ columns: ["invoice_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX invoices_active_period_claim ON zeroship.invoices USING btree (creator_id, period) WHERE ((status)::text <> 'void'::text)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX migrated_app_policies_app_submitted_idx ON zeroship.migrated_app_policies USING btree (app_id, submitted_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("migrated_migration_audit", { schema: "zeroship" }).index("migrated_migration_audit_app_idx").add({ columns: ["app_id", "migration_id", "created_at"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX migrated_migrations_app_status_idx ON zeroship.migrated_migrations USING btree (app_id, status, submitted_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("oauth_authorization_codes", { schema: "zeroship" }).index("oauth_authorization_codes_expires_at_idx").add({ columns: ["expires_at"] });
  table("oauth_grants", { schema: "zeroship" }).index("oauth_grants_client_idx").add({ columns: ["client_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX oauth_grants_user_granted_idx ON zeroship.oauth_grants USING btree (user_id, granted_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("oauth_refresh_tokens", { schema: "zeroship" }).index("oauth_refresh_tokens_expires_at_idx").add({ columns: ["expires_at"] });
  table("oauth_refresh_tokens", { schema: "zeroship" }).index("oauth_refresh_tokens_family_idx").add({ columns: ["refresh_family_id", "client_id", "user_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX oauth_refresh_tokens_idem_reap_idx ON zeroship.oauth_refresh_tokens USING btree (idem_expires_at) WHERE (idem_response_enc IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE UNIQUE INDEX oauth_refresh_tokens_one_active_per_family ON zeroship.oauth_refresh_tokens USING btree (refresh_family_id) WHERE ((rotated_at IS NULL) AND (revoked_at IS NULL))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("oauth_refresh_tokens", { schema: "zeroship" }).index("oauth_refresh_tokens_user_idx").add({ columns: ["user_id"] });
  table("oidc_session_clients", { schema: "zeroship" }).index("oidc_session_clients_client_idx").add({ columns: ["client_id"] });
  table("oidc_session_clients", { schema: "zeroship" }).index("oidc_session_clients_user_idx").add({ columns: ["user_id", "idp_session_id"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX payout_failures_creator_idx ON zeroship.payout_failures USING btree (creator_id, created_at DESC)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX pending_disputes_charge_idx ON zeroship.pending_disputes USING btree (charge) WHERE (charge IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX pending_disputes_payment_intent_idx ON zeroship.pending_disputes USING btree (payment_intent) WHERE (payment_intent IS NOT NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX permission_tokens_owner_active_idx ON zeroship.permission_tokens USING btree (owner_id) WHERE (revoked_at IS NULL)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX permission_tokens_policies_gin_idx ON zeroship.permission_tokens USING gin (policies)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("plan_change_events", { schema: "zeroship" }).index("plan_change_events_app_period_idx").add({ columns: ["app_id", "period"] });
  table("refunds", { schema: "zeroship" }).index("refunds_invoice_idx").add({ columns: ["invoice_id"] });
  table("sandbox_events_2026_05", { schema: "zeroship" }).index("sandbox_events_2026_05_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_ts_idx ON zeroship.sandbox_events_2026_05 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_05_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_05 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_05", { schema: "zeroship" }).index("sandbox_events_2026_05_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_2026_06", { schema: "zeroship" }).index("sandbox_events_2026_06_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_ts_idx ON zeroship.sandbox_events_2026_06 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_06_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_06 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_06", { schema: "zeroship" }).index("sandbox_events_2026_06_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_2026_07", { schema: "zeroship" }).index("sandbox_events_2026_07_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_ts_idx ON zeroship.sandbox_events_2026_07 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_07_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_07 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_07", { schema: "zeroship" }).index("sandbox_events_2026_07_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_2026_08", { schema: "zeroship" }).index("sandbox_events_2026_08_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_ts_idx ON zeroship.sandbox_events_2026_08 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_08_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_08 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_08", { schema: "zeroship" }).index("sandbox_events_2026_08_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_2026_09", { schema: "zeroship" }).index("sandbox_events_2026_09_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_ts_idx ON zeroship.sandbox_events_2026_09 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_09_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_09 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_09", { schema: "zeroship" }).index("sandbox_events_2026_09_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_2026_10", { schema: "zeroship" }).index("sandbox_events_2026_10_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_ts_idx ON zeroship.sandbox_events_2026_10 USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_2026_10_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_10 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_2026_10", { schema: "zeroship" }).index("sandbox_events_2026_10_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  table("sandbox_events_default", { schema: "zeroship" }).index("sandbox_events_default_sandbox_id_ts_idx").add({ columns: ["sandbox_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_default_ts_idx ON zeroship.sandbox_events_default USING brin (ts) WITH (pages_per_range='32')", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandbox_events_default_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_default USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("sandbox_events_default", { schema: "zeroship" }).index("sandbox_events_default_user_id_ts_idx").add({ columns: ["user_id", "ts"] });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandboxes_idle_snapshot_idx ON zeroship.sandboxes USING btree (last_used_at) WHERE ((status = 'running'::text) AND idle_snapshot_opted_in)", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  // TODO(dsl-v2): rich index features need structural support (partial predicates, sort direction, INCLUDE, ONLY, BRIN/WITH, or expression predicates)
  raw({ sql: "CREATE INDEX sandboxes_status_lessee_idx ON zeroship.sandboxes USING btree (status, lessee_updated_at) WHERE (status = ANY (ARRAY['snapshotting'::text, 'restoring'::text, 'restoring_cold'::text]))", reason: "this index uses PostgreSQL index features outside the current structural index renderer" });
  table("signing_keys", { schema: "zeroship" }).index("signing_keys_status_idx").add({ columns: ["status"] });
  table("usage_aggregates", { schema: "zeroship" }).index("usage_aggregates_period_idx").add({ columns: ["period", "app_id"] });
  table("wake_jobs", { schema: "zeroship" }).index("wake_jobs_lessee_idx").add({ columns: ["lessee_updated_at"], where: (c) => notMembership(c("state"), ["ok", "failed"]) });
  table("wake_jobs", { schema: "zeroship" }).index("wake_jobs_sandbox_idx").add({ columns: ["sandbox_id"] });
  table("wake_jobs", { schema: "zeroship" }).index("wake_jobs_sandbox_pending_uniq").add({ columns: ["sandbox_id"], unique: true, where: (c) => notMembership(c("state"), ["ok", "failed"]) });
  table("wake_jobs", { schema: "zeroship" }).index("wake_jobs_state_idx").add({ columns: ["state"], where: (c) => notMembership(c("state"), ["ok", "failed"]) });
  table("wake_jobs", { schema: "zeroship" }).index("wake_jobs_updated_at_idx").add({ columns: ["updated_at"] });
  table("app_env_expose", { schema: "zeroship" }).foreignKey("app_env_expose_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_members", { schema: "zeroship" }).foreignKey("app_members_added_by_fkey").add({ columns: ["added_by"], references: { table: "users", columns: ["id"] } });
  table("app_members", { schema: "zeroship" }).foreignKey("app_members_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_members", { schema: "zeroship" }).foreignKey("app_members_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("app_net_grants", { schema: "zeroship" }).foreignKey("app_net_grants_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_oauth_clients", { schema: "zeroship" }).foreignKey("app_oauth_clients_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_oauth_clients", { schema: "zeroship" }).addForeignKey("app_oauth_clients_client_id_fkey", { columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("app_scope_defs", { schema: "zeroship" }).foreignKey("app_scope_defs_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_secrets", { schema: "zeroship" }).foreignKey("app_secrets_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_session_anchors", { schema: "zeroship" }).foreignKey("app_session_anchors_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_session_anchors", { schema: "zeroship" }).addForeignKey("app_session_anchors_client_id_fkey", { columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("app_session_anchors", { schema: "zeroship" }).foreignKey("app_session_anchors_global_user_id_fkey").add({ columns: ["global_user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("app_spend_limit", { schema: "zeroship" }).foreignKey("app_spend_limit_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_spend_state", { schema: "zeroship" }).foreignKey("app_spend_state_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_usage", { schema: "zeroship" }).foreignKey("app_usage_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_usage_history", { schema: "zeroship" }).foreignKey("app_usage_history_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("app_user_identities", { schema: "zeroship" }).addForeignKey("app_user_identities_app_client_id_fkey", { columns: ["app_client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("app_user_identities", { schema: "zeroship" }).foreignKey("app_user_identities_global_user_id_fkey").add({ columns: ["global_user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("app_vars", { schema: "zeroship" }).foreignKey("app_vars_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("apps", { schema: "zeroship" }).foreignKey("apps_plan_fk").add({ columns: ["plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
  table("billing_customer_refs", { schema: "zeroship" }).addForeignKey("billing_customer_refs_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_billing", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("billing_disputes", { schema: "zeroship" }).foreignKey("billing_disputes_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
  table("billing_line_provider_refs", { schema: "zeroship" }).addForeignKey("billing_line_provider_refs_line_fk", { columns: ["invoice_id", "app_id", "segment_no"], references: { table: "invoice_lines", columns: ["invoice_id", "app_id", "segment_no"], schema: "zeroship" }, onDelete: "cascade" });
  table("billing_metrics", { schema: "zeroship" }).foreignKey("billing_metrics_owner_app_fkey").add({ columns: ["owner_app"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("billing_notifications", { schema: "zeroship" }).addForeignKey("billing_notifications_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_billing", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("billing_provider_refs", { schema: "zeroship" }).foreignKey("billing_provider_refs_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "cascade" });
  table("connect_checkout_failures", { schema: "zeroship" }).addForeignKey("connect_checkout_failures_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_accounts", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("creator_accounts", { schema: "zeroship" }).foreignKey("creator_accounts_creator_id_fkey").add({ columns: ["creator_id"], references: { table: "users", columns: ["id"] } });
  table("creator_billing", { schema: "zeroship" }).foreignKey("creator_billing_creator_id_fkey").add({ columns: ["creator_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("creator_billing_status", { schema: "zeroship" }).addForeignKey("creator_billing_status_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_billing", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("creator_billing_status_history", { schema: "zeroship" }).addForeignKey("creator_billing_status_history_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_billing", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("creator_fee_policy", { schema: "zeroship" }).foreignKey("creator_fee_policy_creator_id_fkey").add({ columns: ["creator_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("credit_ledger", { schema: "zeroship" }).foreignKey("credit_ledger_applied_invoice_id_fkey").add({ columns: ["applied_invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
  table("credit_ledger", { schema: "zeroship" }).foreignKey("credit_ledger_consumed_from_grant_id_fkey").add({ columns: ["consumed_from_grant_id"], references: { table: "credit_ledger", columns: ["id"] }, onDelete: "restrict" });
  table("credit_ledger", { schema: "zeroship" }).addForeignKey("credit_ledger_creator_id_fkey", { columns: ["creator_id"], references: { table: "creator_billing", columns: ["creator_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("device_grants", { schema: "zeroship" }).addForeignKey("device_grants_client_id_fkey", { columns: ["client_id"], references: { table: "oauth_clients", columns: ["client_id"], schema: "zeroship" }, onDelete: "cascade" });
  table("device_grants", { schema: "zeroship" }).foreignKey("device_grants_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] } });
  table("email_verifications", { schema: "zeroship" }).foreignKey("email_verifications_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("federated_identities", { schema: "zeroship" }).foreignKey("federated_identities_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("gateway_sessions", { schema: "zeroship" }).foreignKey("gateway_sessions_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("identity_links", { schema: "zeroship" }).foreignKey("identity_links_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] } });
  table("idp_sessions", { schema: "zeroship" }).foreignKey("idp_sessions_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "restrict" });
  table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
  table("invoice_lines", { schema: "zeroship" }).foreignKey("invoice_lines_plan_id_fkey").add({ columns: ["plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
  table("invoice_payments", { schema: "zeroship" }).foreignKey("invoice_payments_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices\n    ADD CONSTRAINT invoices_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("magic_links", { schema: "zeroship" }).foreignKey("magic_links_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports\n    ADD CONSTRAINT metering_exports_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.metric_weights\n    ADD CONSTRAINT metric_weights_metric_fkey FOREIGN KEY (metric) REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT", reason: "the current basic FK path is limited to single-column references to id" });
  table("migrated_app_policies", { schema: "zeroship" }).foreignKey("migrated_app_policies_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("migrated_app_policies", { schema: "zeroship" }).foreignKey("migrated_app_policies_submitted_by_fkey").add({ columns: ["submitted_by"], references: { table: "users", columns: ["id"] }, onDelete: "restrict" });
  table("migrated_migration_audit", { schema: "zeroship" }).foreignKey("migrated_migration_audit_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("migrated_migration_audit", { schema: "zeroship" }).foreignKey("migrated_migration_audit_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] }, onDelete: "restrict" });
  table("migrated_migrations", { schema: "zeroship" }).foreignKey("migrated_migrations_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("migrated_migrations", { schema: "zeroship" }).foreignKey("migrated_migrations_approved_by_fkey").add({ columns: ["approved_by"], references: { table: "users", columns: ["id"] }, onDelete: "restrict" });
  table("migrated_migrations", { schema: "zeroship" }).foreignKey("migrated_migrations_submitted_by_fkey").add({ columns: ["submitted_by"], references: { table: "users", columns: ["id"] }, onDelete: "restrict" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes\n    ADD CONSTRAINT oauth_authorization_codes_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("oauth_authorization_codes", { schema: "zeroship" }).foreignKey("oauth_authorization_codes_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("oauth_clients", { schema: "zeroship" }).foreignKey("oauth_clients_created_by_fkey").add({ columns: ["created_by"], references: { table: "users", columns: ["id"] } });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_grants\n    ADD CONSTRAINT oauth_grants_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("oauth_grants", { schema: "zeroship" }).foreignKey("oauth_grants_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens\n    ADD CONSTRAINT oauth_refresh_tokens_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("oauth_refresh_tokens", { schema: "zeroship" }).foreignKey("oauth_refresh_tokens_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.oidc_session_clients\n    ADD CONSTRAINT oidc_session_clients_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("oidc_session_clients", { schema: "zeroship" }).foreignKey("oidc_session_clients_idp_session_id_fkey").add({ columns: ["idp_session_id"], references: { table: "idp_sessions", columns: ["id"] }, onDelete: "cascade" });
  table("oidc_session_clients", { schema: "zeroship" }).foreignKey("oidc_session_clients_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures\n    ADD CONSTRAINT payout_failures_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts\n    ADD CONSTRAINT payouts_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE RESTRICT", reason: "the current basic FK path is limited to single-column references to id" });
  table("permission_tokens", { schema: "zeroship" }).foreignKey("permission_tokens_owner_id_fkey").add({ columns: ["owner_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_from_plan_id_fkey").add({ columns: ["from_plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
  table("plan_change_events", { schema: "zeroship" }).foreignKey("plan_change_events_to_plan_id_fkey").add({ columns: ["to_plan_id"], references: { table: "plans", columns: ["id"] }, onDelete: "restrict" });
  table("platform_admin_roles", { schema: "zeroship" }).foreignKey("platform_admin_roles_granted_by_fkey").add({ columns: ["granted_by"], references: { table: "users", columns: ["id"] } });
  table("platform_admin_roles", { schema: "zeroship" }).foreignKey("platform_admin_roles_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("platform_policies", { schema: "zeroship" }).foreignKey("platform_policies_updated_by_fkey").add({ columns: ["updated_by"], references: { table: "users", columns: ["id"] } });
  table("principal_grants", { schema: "zeroship" }).foreignKey("principal_grants_principal_id_fkey").add({ columns: ["principal_id"], references: { table: "users", columns: ["id"] } });
  table("refund_provider_refs", { schema: "zeroship" }).foreignKey("refund_provider_refs_refund_id_fkey").add({ columns: ["refund_id"], references: { table: "refunds", columns: ["id"] }, onDelete: "cascade" });
  table("refunds", { schema: "zeroship" }).foreignKey("refunds_invoice_id_fkey").add({ columns: ["invoice_id"], references: { table: "invoices", columns: ["id"] }, onDelete: "restrict" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes\n    ADD CONSTRAINT sandboxes_host_id_fkey FOREIGN KEY (host_id) REFERENCES zeroship.hosts(host_id) ON DELETE RESTRICT", reason: "the current basic FK path is limited to single-column references to id" });
  // TODO(dsl-v2): multi-column or non-id foreign keys remain raw until the FK lowerer accepts the full constraint model
  raw({ sql: "ALTER TABLE ONLY zeroship.shares\n    ADD CONSTRAINT shares_sandbox_id_fkey FOREIGN KEY (sandbox_id) REFERENCES zeroship.sandboxes(sandbox_id) ON DELETE CASCADE", reason: "the current basic FK path is limited to single-column references to id" });
  table("spend_state_history", { schema: "zeroship" }).foreignKey("spend_state_history_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  table("totp_backup_codes", { schema: "zeroship" }).foreignKey("totp_backup_codes_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("totp_credentials", { schema: "zeroship" }).foreignKey("totp_credentials_user_id_fkey").add({ columns: ["user_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
  table("usage_aggregates", { schema: "zeroship" }).foreignKey("usage_aggregates_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
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
