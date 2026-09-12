import { raw } from "@zeroship/migrate";

// These are the platform columns whose entire semantic domain is a canonical
// case-sensitive typed id or a storage copy of one. PostgreSQL's locale
// collation does not keep the base36 alphabet in numeric order, so sortable
// entity ids need bytewise ordering. Their foreign-key and denormalized copies
// need the same collation even when they are never ordered: otherwise a join
// against the collated entity id cannot use the copy's ordinary index.
//
// This map is semantic, not name-based. It deliberately excludes raw UUID
// domains;
// pricing_config.id and workflow_rollout_config.id (the constant "global");
// OAuth client ids, provider ids, hashes, idempotency keys, boot ids, snapshot
// and worker ids, the URL-safe share token id, and arbitrary text. Some included
// ids, such as built-in plan ids and sandbox project ids, are deterministically
// UUID-derived rather than UUIDv7; bytewise comparison is still the canonical
// identity-domain rule, but those particular values do not encode creation time.
const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  // Control and billing.
  app_audit: ["creator_id", "actor_user_id"],
  app_egress_rules: ["created_by"],
  app_members: ["user_id", "added_by"],
  app_schema_applies: ["submitted_by"],
  apps: ["plan_id"],
  billing_customer_refs: ["creator_id"],
  billing_disputes: ["id", "invoice_id"],
  billing_line_provider_refs: ["invoice_id"],
  billing_notifications: ["creator_id", "transition_id"],
  billing_provider_refs: ["invoice_id"],
  billing_reconciliation_findings: ["id"],
  connect_checkout_failures: ["id", "creator_id"],
  creator_account_history: ["creator_id"],
  creator_accounts: ["creator_id"],
  creator_billing: ["creator_id"],
  creator_billing_status_history: ["id", "creator_id"],
  creator_billing_status: ["creator_id", "failed_invoice_id"],
  creator_fee_policy: ["creator_id"],
  credit_ledger: ["id", "creator_id", "applied_invoice_id", "consumed_from_grant_id"],
  invoice_lines: ["invoice_id", "plan_id"],
  invoice_payments: ["id", "invoice_id"],
  invoices: ["id", "creator_id"],
  payout_failures: ["id", "creator_id"],
  payouts: ["creator_id"],
  plan_change_events: ["id", "from_plan_id", "to_plan_id"],
  plans: ["id"],
  provider_dead_letter: ["id"],
  refund_provider_refs: ["refund_id"],
  refunds: ["id", "invoice_id"],
  spend_state_history: ["id"],

  // Auth identity.
  app_session_anchors: ["global_user_id", "refresh_family_id"],
  app_user_identities: ["global_user_id"],
  audit_events: ["actor_user_id"],
  authz_decisions: ["actor_user_id"],
  device_grants: ["principal_id"],
  email_verifications: ["user_id"],
  federated_identities: ["user_id"],
  gateway_sessions: ["user_id"],
  identity_links: ["principal_id"],
  idp_sessions: ["user_id"],
  magic_links: ["user_id"],
  oauth_authorization_codes: ["user_id"],
  oauth_clients: ["created_by"],
  oauth_grants: ["user_id"],
  oauth_refresh_tokens: ["user_id", "refresh_family_id"],
  oidc_session_clients: ["user_id"],
  principal_grants: ["principal_id"],
  totp_backup_codes: ["user_id"],
  totp_credentials: ["user_id"],
  users: ["id"],

  // Workflow identities and immutable deployment references.
  app_deploys: ["id"],
  app_deploy_holds: ["deploy_id", "holder_id"],
  workflow_broadcasts: ["id", "deploy_id"],
  workflow_runs: ["id", "dispatch_nonce", "parent_run_id", "deploy_id"],
  workflow_schedules: ["id", "deploy_id"],
  workflow_scheduler_inflight: ["run_id"],
  workflow_scheduler_timers: ["run_id"],
  workflow_signal_keys: ["id", "kid"],
  workflow_signals: ["id", "run_id", "broadcast_id"],
  workflow_steps: [
    "run_id",
    "consumed_signal_id",
    "child_run_id",
    "batch_id",
    "compensation_batch_id",
  ],
  workflow_subscriptions: ["id", "run_id"],

  // Sandbox typed-id world. Partition children inherit the sandbox_events
  // column collations from the partitioned parent.
  deleted_sandboxes: ["sandbox_id", "user_id"],
  hosts: ["host_id"],
  sandbox_events: ["event_id", "sandbox_id", "user_id"],
  sandboxes: ["sandbox_id", "user_id", "project_id", "host_id"],
  shares: ["sandbox_id", "iss"],
  wake_jobs: ["wake_id", "sandbox_id"],
};

export default {
  name: "sortable_entity_id_collations",
  schema() {
    for (const [table, columns] of Object.entries(typedIdColumnsByTable)) {
      const alterations = columns
        .map((column) => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`)
        .join(", ");
      raw({
        sql: `ALTER TABLE "zeroship"."${table}" ${alterations}`,
        reason:
          "typed-id text domains need bytewise comparison, including matching copies used by indexed joins",
      });
    }
  },
};
