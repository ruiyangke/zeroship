import { table, t, now, genRandomUuid } from "@zeroship/migrate";

export const name = "control_tables";

export function up() {
  table("app_audit", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      app_id: t.uuid(),
      creator_id: t.uuid(),
      actor_user_id: t.uuid(),
      actor_token_id: t.uuid(),
      action: t.text().notNull(),
      resource: t.text(),
      source_ip: t.inet(),
      detail: t.json(),
      occurred_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("app_env_expose", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      key_name: t.text().notNull(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id", "key_name"],
  });
  table("app_members", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      user_id: t.uuid().notNull(),
      role: t.text().notNull(),
      added_at: t.timestamp().notNull().default(now()),
      added_by: t.uuid(),
    },
    primaryKey: ["app_id", "user_id"],
  });
  table("app_members", { schema: "zeroship" }).check("app_members_role_check").add({ expr: (col) => col("role").in(["owner", "editor", "viewer"]) });
  table("app_net_grants", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      host: t.text().notNull(),
      port: t.int().notNull(),
      granted_by: t.text().notNull(),
      granted_at: t.timestamp().notNull().default(now()),
      note: t.text(),
    },
    primaryKey: ["app_id", "host", "port"],
  });
  table("app_net_grants", { schema: "zeroship" }).check("app_net_grants_port_check").add({ expr: (col) => col("port").ge(1).and(col("port").le(65535)) });
  table("app_oauth_clients", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      client_id: t.text().notNull(),
      sector_identifier: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
      updated_at: t.timestamp().notNull().default(now()),
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
      updated_at: t.timestamp().notNull().default(now()),
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
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: null,
  });
  table("app_vars", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      key_name: t.text().notNull(),
      value: t.text().notNull(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id", "key_name"],
  });
  table("apps", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      name: t.text().notNull(),
      plan_id: t.text().notNull().default("free"),
      deploy_hash: t.text(),
      api_key: t.text().notNull(),
      api_key_hash: t.text().notNull().default(""),
      env_version: t.bigInt().notNull().default(0),
      suspended: t.boolean().notNull().default(false),
      audit_locked: t.boolean().notNull().default(false),
      workflows_enabled: t.boolean().notNull().default(false),
      manifest_json: t.text(),
      created_at: t.timestamp().notNull().default(now()),
      updated_at: t.timestamp().notNull().default(now()),
      system: t.boolean().notNull().default(false),
    },
    primaryKey: ["id"],
  });
  table("creator_account_history", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      creator_id: t.uuid().notNull(),
      stripe_account_id: t.text().notNull(),
      linked_at: t.timestamp().notNull().default(now()),
      unlinked_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("creator_accounts", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      stripe_account_id: t.text().notNull(),
      onboarded_at: t.timestamp().notNull().default(now()),
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
      submitted_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id", "version"],
  });
  table("migrated_app_policies", { schema: "zeroship" }).check("migrated_app_policies_ceiling_version_check").add({ expr: (col) => col("ceiling_version").gt(0) });
  table("migrated_app_policies", { schema: "zeroship" }).check("migrated_app_policies_version_check").add({ expr: (col) => col("version").gt(0) });
  table("migrated_migration_audit", { schema: "zeroship" }).create({
    columns: {
      audit_id: t.uuid().notNull().default(genRandomUuid()),
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
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["audit_id"],
  });
  table("migrated_migration_audit", { schema: "zeroship" }).check("migrated_migration_audit_action_check").add({ expr: (col) => col("action").in(["submit", "reject_pending", "approve", "apply"]) });
  table("migrated_migration_audit", { schema: "zeroship" }).check("migrated_migration_audit_ceiling_version_check").add({ expr: (col) => col("ceiling_version").gt(0) });
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
      submitted_at: t.timestamp().notNull().default(now()),
      approved_by: t.uuid(),
      approved_at: t.timestamp(),
      applied_at: t.timestamp(),
      // The content checksum the operator reviewed at approve() — the TOCTOU pin the
      // apply gate re-verifies against the re-resolved migration set. NULL until the
      // record is approved (auto-approved or operator-approved).
      approved_checksum: t.text(),
      last_error: t.text(),
    },
    primaryKey: ["app_id", "migration_id"],
  });
  table("migrated_migrations", { schema: "zeroship" }).check("migrated_migrations_ceiling_version_check").add({ expr: (col) => col("ceiling_version").gt(0) });
  table("migrated_migrations", { schema: "zeroship" }).check("migrated_migrations_status_check").add({ expr: (col) => col("status").in(["planned", "pending_approval", "approved", "applied", "rejected"]) });
  table("net_policy_catalog", { schema: "zeroship" }).create({
    columns: {
      key: t.text().notNull(),
      value_json: t.json().notNull(),
      updated_by: t.text(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["key"],
  });
  table("payouts", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      creator_id: t.uuid().notNull(),
      event_id: t.text().notNull(),
      event_type: t.text().notNull(),
      gross_amount: t.bigInt().notNull(),
      platform_fee: t.bigInt().notNull(),
      net_amount: t.bigInt().notNull(),
      currency: t.text().notNull(),
      occurred_at: t.timestamp().notNull(),
      payload_hash: t.bytes(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("payouts", { schema: "zeroship" }).check("control_payouts_currency_shape").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("payouts", { schema: "zeroship" }).check("control_payouts_fee_lte_gross").add({ expr: (col) => col("platform_fee").le(col("gross_amount")) });
  table("payouts", { schema: "zeroship" }).check("control_payouts_fee_nonnegative").add({ expr: (col) => col("platform_fee").ge(0) });
  table("payouts", { schema: "zeroship" }).check("control_payouts_gross_nonnegative").add({ expr: (col) => col("gross_amount").ge(0) });
  table("payouts", { schema: "zeroship" }).check("control_payouts_net_matches_amounts").add({ expr: (col) => col("net_amount").eq(col("gross_amount").sub(col("platform_fee"))) });
  table("payouts", { schema: "zeroship" }).check("control_payouts_net_nonnegative").add({ expr: (col) => col("net_amount").ge(0) });
  table("permission_tokens", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull(),
      owner_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      client_id: t.text(),
      name: t.text().notNull(),
      policies: t.json().notNull(),
      policy_hash: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
      expires_at: t.timestamp(),
      revoked_at: t.timestamp(),
      last_used_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("permission_tokens", { schema: "zeroship" }).check("permission_tokens_kind_check").add({ expr: (col) => col("kind").in(["pat", "oauth_grant"]) });
  table("platform_admin_roles", { schema: "zeroship" }).create({
    columns: {
      user_id: t.uuid().notNull(),
      role: t.text().notNull(),
      granted_at: t.timestamp().notNull().default(now()),
      granted_by: t.uuid(),
    },
    primaryKey: ["user_id"],
  });
  table("platform_admin_roles", { schema: "zeroship" }).check("platform_admin_roles_role_check").add({ expr: (col) => col("role").in(["admin", "support", "billing", "readonly"]) });
  table("platform_policies", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      cedar_source: t.text().notNull(),
      enabled: t.boolean().notNull().default(true),
      updated_at: t.timestamp().notNull().default(now()),
      updated_by: t.uuid(),
    },
    primaryKey: ["id"],
  });
}

export function down() {

}
