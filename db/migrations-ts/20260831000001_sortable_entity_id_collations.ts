import { raw } from "@zeroship/migrate";

// These are the platform columns whose entire semantic domain is a canonical
// case-sensitive typed id or a storage copy of one. PostgreSQL's locale
// collation does not keep the base62 alphabet in numeric order, so sortable
// entity ids need bytewise ordering. Their foreign-key and denormalized copies
// need the same collation even when they are never ordered: otherwise a join
// against the collated entity id cannot use the copy's ordinary index.
//
// This map is semantic, not name-based. It deliberately excludes raw UUID
// domains; app_deploys.id (dep_ plus UUIDv4 hex) and its deploy_id copies;
// pricing_config.id and workflow_rollout_config.id (the constant "global");
// OAuth client ids, provider ids, hashes, idempotency keys, boot ids, snapshot
// and worker ids, the URL-safe share token id, and arbitrary text. Some included
// ids, such as built-in plan ids and sandbox project ids, are deterministically
// UUID-derived rather than UUIDv7; bytewise comparison is still the canonical
// identity-domain rule, but those particular values do not encode creation time.
const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  // Control and billing: 28 columns.
  apps: ["plan_id"],
  billing_disputes: ["id", "invoice_id"],
  billing_line_provider_refs: ["invoice_id"],
  billing_notifications: ["transition_id"],
  billing_provider_refs: ["invoice_id"],
  billing_reconciliation_findings: ["id"],
  connect_checkout_failures: ["id"],
  creator_billing_status_history: ["id"],
  creator_billing_status: ["failed_invoice_id"],
  credit_ledger: ["id", "applied_invoice_id", "consumed_from_grant_id"],
  invoice_lines: ["invoice_id", "plan_id"],
  invoice_payments: ["id", "invoice_id"],
  invoices: ["id"],
  payout_failures: ["id"],
  plan_change_events: ["id", "from_plan_id", "to_plan_id"],
  plans: ["id"],
  provider_dead_letter: ["id"],
  refund_provider_refs: ["refund_id"],
  refunds: ["id", "invoice_id"],
  spend_state_history: ["id"],

  // Auth refresh-family identity: 2 columns.
  app_session_anchors: ["refresh_family_id"],
  oauth_refresh_tokens: ["refresh_family_id"],

  // Durable workflows: 19 columns.
  workflow_broadcasts: ["id"],
  workflow_runs: ["id", "dispatch_nonce", "parent_run_id"],
  workflow_schedules: ["id"],
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

  // Sandbox typed-id world: 14 columns. Partition children inherit the
  // sandbox_events column collations from the partitioned parent.
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
