import { and, membership, table, t } from "@zeroship/migrate";
export const name = "control_tables";

export function up() {
  table("app_audit", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      app_id: t.uuid(),
      creator_id: t.uuid(),
      actor_user_id: t.uuid(),
      actor_token_id: t.uuid(),
      action: t.text().notNull(),
      resource: t.text(),
      source_ip: t.inet(),
      detail: t.json(),
      occurred_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("app_env_expose", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      key_name: t.text().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "key_name"],
  });
  table("app_members", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      user_id: t.uuid().notNull(),
      role: t.text().notNull(),
      added_at: t.timestamp().notNull().default({ fn: "now" }),
      added_by: t.uuid(),
    },
    primaryKey: ["app_id", "user_id"],
  });
  table("app_members", { schema: "zeroship" }).addCheck("app_members_role_check", (c) => membership(c("role"), ["owner", "editor", "viewer"]));
  table("app_net_grants", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      host: t.text().notNull(),
      port: t.integer().notNull(),
      granted_by: t.text().notNull(),
      granted_at: t.timestamp().notNull().default({ fn: "now" }),
      note: t.text(),
    },
    primaryKey: ["app_id", "host", "port"],
  });
  table("app_net_grants", { schema: "zeroship" }).addCheck("app_net_grants_port_check", (c) => and(c("port").ge(1), c("port").le(65535)));
  table("app_oauth_clients", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      client_id: t.text().notNull(),
      sector_identifier: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id"],
  });
  table("app_scope_defs", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      scope_id: t.text().notNull(),
      label: t.text().notNull(),
      description: t.text(),
    },
    primaryKey: ["app_id", "scope_id"],
  });
  table("app_secrets", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      key_name: t.text().notNull(),
      ciphertext: t.bytes().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "key_name"],
  });
  table("app_usage", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      resource: t.text().notNull(),
      value: t.bigInt().notNull().default(0),
    },
    primaryKey: ["app_id", "resource"],
  });
  table("app_usage_history", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      period: t.text().notNull(),
      counters: t.json().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: null,
  });
  table("app_vars", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      key_name: t.text().notNull(),
      value: t.text().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "key_name"],
  });
  table("apps", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      name: t.text().notNull(),
      plan_id: t.text().notNull().default("free"),
      deploy_hash: t.text(),
      api_key: t.text().notNull(),
      api_key_hash: t.text().notNull().default(""),
      env_version: t.bigInt().notNull().default(0),
      suspended: t.boolean().notNull().default(false),
      audit_locked: t.boolean().notNull().default(false),
      manifest_json: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      system: t.boolean().notNull().default(false),
    },
    primaryKey: ["id"],
  });
  table("creator_account_history", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      creator_id: t.uuid().notNull(),
      stripe_account_id: t.text().notNull(),
      linked_at: t.timestamp().notNull().default({ fn: "now" }),
      unlinked_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("creator_accounts", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      stripe_account_id: t.text().notNull(),
      onboarded_at: t.timestamp().notNull().default({ fn: "now" }),
      unlinked_at: t.timestamp(),
      charges_enabled: t.boolean().notNull().default(false),
      payouts_enabled: t.boolean().notNull().default(false),
      details_submitted: t.boolean().notNull().default(false),
    },
    primaryKey: ["creator_id"],
  });
  table("migrated_app_policies", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      version: t.bigInt().notNull(),
      raw_toml: t.text().notNull(),
      parsed_profile: t.json().notNull(),
      effective_profile: t.json().notNull(),
      ceiling_id: t.text().notNull(),
      ceiling_version: t.bigInt().notNull(),
      submitted_by: t.uuid().notNull(),
      submitted_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "version"],
  });
  table("migrated_app_policies", { schema: "zeroship" }).addCheck("migrated_app_policies_ceiling_version_check", (c) => c("ceiling_version").gt(0));
  table("migrated_app_policies", { schema: "zeroship" }).addCheck("migrated_app_policies_version_check", (c) => c("version").gt(0));
  table("migrated_migration_audit", { schema: "zeroship" }).create({
    columns: {
      audit_id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      app_id: t.uuid().notNull(),
      migration_id: t.uuid().notNull(),
      migration_versions: t.json().notNull().default([]),
      action: t.text().notNull(),
      outcome: t.text().notNull(),
      principal_id: t.uuid().notNull(),
      effective_profile: t.json().notNull(),
      sealed_profile: t.json(),
      ceiling_id: t.text().notNull(),
      ceiling_version: t.bigInt().notNull(),
      detail: t.json().notNull().default({}),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["audit_id"],
  });
  table("migrated_migration_audit", { schema: "zeroship" }).addCheck("migrated_migration_audit_action_check", (c) => membership(c("action"), ["submit", "reject_pending", "approve", "apply"]));
  table("migrated_migration_audit", { schema: "zeroship" }).addCheck("migrated_migration_audit_ceiling_version_check", (c) => c("ceiling_version").gt(0));
  table("migrated_migrations", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      migration_id: t.uuid().notNull(),
      status: t.text().notNull(),
      request_body: t.json().notNull(),
      effective_profile: t.json().notNull(),
      ceiling_id: t.text().notNull(),
      ceiling_version: t.bigInt().notNull(),
      gated_versions: t.json().notNull().default([]),
      submitted_by: t.uuid().notNull(),
      submitted_at: t.timestamp().notNull().default({ fn: "now" }),
      approved_by: t.uuid(),
      approved_at: t.timestamp(),
      applied_at: t.timestamp(),
      last_error: t.text(),
    },
    primaryKey: ["app_id", "migration_id"],
  });
  table("migrated_migrations", { schema: "zeroship" }).addCheck("migrated_migrations_ceiling_version_check", (c) => c("ceiling_version").gt(0));
  table("migrated_migrations", { schema: "zeroship" }).addCheck("migrated_migrations_status_check", (c) => membership(c("status"), ["submitted", "pending_approval", "approved", "applied", "failed"]));
  table("net_policy_catalog", { schema: "zeroship" }).create({
    columns: {
      key: t.text().notNull(),
      value_json: t.json().notNull(),
      updated_by: t.text(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["key"],
  });
  table("payouts", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      creator_id: t.uuid().notNull(),
      event_id: t.text().notNull(),
      event_type: t.text().notNull(),
      gross_amount: t.bigInt().notNull(),
      platform_fee: t.bigInt().notNull(),
      net_amount: t.bigInt().notNull(),
      currency: t.text().notNull(),
      occurred_at: t.timestamp().notNull(),
      payload_hash: t.bytes(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_currency_shape", (c) => c("currency").matches("^[a-z]{3}$"));
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_fee_lte_gross", (c) => c("platform_fee").le(c("gross_amount")));
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_fee_nonnegative", (c) => c("platform_fee").ge(0));
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_gross_nonnegative", (c) => c("gross_amount").ge(0));
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_net_matches_amounts", (c) => c("net_amount").eq(c("gross_amount").sub(c("platform_fee"))));
  table("payouts", { schema: "zeroship" }).addCheck("control_payouts_net_nonnegative", (c) => c("net_amount").ge(0));
  table("permission_tokens", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull(),
      owner_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      client_id: t.text(),
      name: t.text().notNull(),
      policies: t.json().notNull(),
      policy_hash: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp(),
      revoked_at: t.timestamp(),
      last_used_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("permission_tokens", { schema: "zeroship" }).addCheck("permission_tokens_kind_check", (c) => membership(c("kind"), ["pat", "oauth_grant"]));
  table("platform_admin_roles", { schema: "zeroship" }).create({
    columns: {
      user_id: t.uuid().notNull(),
      role: t.text().notNull(),
      granted_at: t.timestamp().notNull().default({ fn: "now" }),
      granted_by: t.uuid(),
    },
    primaryKey: ["user_id"],
  });
  table("platform_admin_roles", { schema: "zeroship" }).addCheck("platform_admin_roles_role_check", (c) => membership(c("role"), ["admin", "support", "billing", "readonly"]));
  table("platform_policies", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      cedar_source: t.text().notNull(),
      enabled: t.boolean().notNull().default(true),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_by: t.uuid(),
    },
    primaryKey: ["id"],
  });
}

export function down() {

}
