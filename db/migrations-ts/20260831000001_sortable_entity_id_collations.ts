import { raw } from "@zeroship/migrate";

// These are the platform columns whose entire semantic domain is a canonical
// case-sensitive typed id or a storage copy of one. PostgreSQL's locale
// collation does not keep the base36 alphabet in numeric order, so sortable
// entity ids need bytewise ordering. Their foreign-key and denormalized copies
// need the same collation even when they are never ordered: otherwise a join
// against the collated entity id cannot use the copy's ordinary index.
//
// This map is semantic, not name-based. It deliberately excludes raw UUID
// domains; pricing_config.id and workflow_rollout_config.id (the constant
// "global"); OAuth client ids, provider ids, hashes, idempotency keys, and
// arbitrary text. Some included ids, such as built-in plan ids, are
// deterministically UUID-derived rather than UUIDv7; bytewise comparison is
// still the canonical identity-domain rule, but those particular values do not
// encode creation time.
//
// This sweep covers only the columns that the migration creating them does not
// already collate; every table named below exists in the final schema.
//
// ONE ENTRY PER TABLE. This is an object literal, so a table named twice keeps
// only the last spelling and silently drops the columns of the first - which is
// the exact failure this map exists to prevent. `apps.id` and every `app_id`
// copy are merged into the group each table already belongs to rather than
// listed as a second app-id group.
const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  // The app identity domain: `apps.id` and every copy of it. `apps.plan_id` is
  // not app identity but is collated here because the map is one entry per
  // table; it belongs to the plan domain (`plans.id` below). This domain is
  // ordered - an app id is a sortable typed id and the control plane pages apps
  // by it - and every copy carries the collation so the paging index is usable
  // from a join.
  apps: ["id", "plan_id"],
  app_audit: ["app_id", "actor_user_id"],
  app_deploys: ["app_id", "id"],
  app_egress_rules: ["app_id", "created_by"],
  app_env_expose: ["app_id"],
  app_oauth_clients: ["app_id"],
  app_scope_defs: ["app_id"],
  app_usage: ["app_id"],
  app_usage_history: ["app_id"],
  app_vars: ["app_id"],
  // `billing_metrics` spells its app reference `owner_app`. The domain is the
  // same and so is the collation; only the column name differs.
  billing_metrics: ["owner_app"],

  // The user identity domain: `users.id` and every copy of it, whatever the
  // copy is named - `principal_id`, `actor_user_id`, `global_user_id`, and the
  // `*_by` columns that carry a user reference are this one domain.
  users: ["id"],
  audit_events: ["actor_user_id"],
  authz_decisions: ["actor_user_id"],
  app_user_identities: ["global_user_id"],
  device_grants: ["principal_id"],
  email_verifications: ["user_id"],
  federated_identities: ["user_id"],
  identity_links: ["principal_id"],
  idp_sessions: ["user_id"],
  magic_links: ["user_id"],
  oauth_authorization_codes: ["user_id"],
  oauth_clients: ["created_by"],
  oauth_grants: ["user_id"],
  oidc_session_clients: ["user_id"],
  principal_grants: ["principal_id"],
  totp_backup_codes: ["user_id"],
  totp_credentials: ["user_id"],

  // Control and billing.
  billing_disputes: ["id", "invoice_id"],
  billing_line_provider_refs: ["app_id", "invoice_id"],
  billing_provider_refs: ["invoice_id"],
  billing_reconciliation_findings: ["id"],
  connect_checkout_failures: ["id"],
  credit_ledger: ["id", "applied_invoice_id", "consumed_from_grant_id"],
  invoice_lines: ["app_id", "invoice_id", "plan_id"],
  invoice_payments: ["id", "invoice_id"],
  invoices: ["id"],
  payout_failures: ["id"],
  plan_change_events: ["id", "from_plan_id", "to_plan_id"],
  plans: ["id"],
  provider_dead_letter: ["id"],
  refund_provider_refs: ["refund_id"],
  refunds: ["id", "invoice_id"],
  spend_state_history: ["id"],

  // Auth refresh-family identity.
  app_session_anchors: ["global_user_id", "refresh_family_id"],
  gateway_sessions: ["user_id"],

  // Workflow identities and immutable deployment references.
  app_deploy_holds: ["app_id", "deploy_id", "holder_id"],
  workflow_policy_ledger: ["id"],
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
