import { table, t } from "@zeroship/migrate";
import { raw } from "@zeroship/migrate/pg";

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
      source_ip: t.text(),
      detail: t.json(),
      occurred_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for inet
  raw({ sql: "ALTER TABLE ONLY zeroship.app_audit ALTER COLUMN source_ip TYPE inet USING source_ip::inet", reason: "column app_audit.source_ip uses PostgreSQL type inet, which is not in the current closed column lexicon/lowerer use-site set" });
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
  // TODO(dsl-v2): CHECK constraint app_members.app_members_role_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.app_members ADD CONSTRAINT app_members_role_check CHECK ((role = ANY (ARRAY['owner'::text, 'editor'::text, 'viewer'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint app_net_grants.app_net_grants_port_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.app_net_grants ADD CONSTRAINT app_net_grants_port_check CHECK (((port >= 1) AND (port <= 65535)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint migrated_app_policies.migrated_app_policies_ceiling_version_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_app_policies ADD CONSTRAINT migrated_app_policies_ceiling_version_check CHECK ((ceiling_version > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint migrated_app_policies.migrated_app_policies_version_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_app_policies ADD CONSTRAINT migrated_app_policies_version_check CHECK ((version > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("migrated_migration_audit", { schema: "zeroship" }).create({
    columns: {
      audit_id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      app_id: t.uuid().notNull(),
      migration_id: t.uuid().notNull(),
      migration_versions: t.json().notNull(),
      action: t.text().notNull(),
      outcome: t.text().notNull(),
      principal_id: t.uuid().notNull(),
      effective_profile: t.json().notNull(),
      sealed_profile: t.json(),
      ceiling_id: t.text().notNull(),
      ceiling_version: t.bigInt().notNull(),
      detail: t.json().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["audit_id"],
  });
  // TODO(dsl-v2): json array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit ALTER COLUMN migration_versions SET DEFAULT '[]'::jsonb", reason: "column migrated_migration_audit.migration_versions requires exact default '[]'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit ALTER COLUMN detail SET DEFAULT '{}'::jsonb", reason: "column migrated_migration_audit.detail requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint migrated_migration_audit.migrated_migration_audit_action_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit ADD CONSTRAINT migrated_migration_audit_action_check CHECK ((action = ANY (ARRAY['submit'::text, 'reject_pending'::text, 'approve'::text, 'apply'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint migrated_migration_audit.migrated_migration_audit_ceiling_version_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migration_audit ADD CONSTRAINT migrated_migration_audit_ceiling_version_check CHECK ((ceiling_version > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("migrated_migrations", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      migration_id: t.uuid().notNull(),
      status: t.text().notNull(),
      request_body: t.json().notNull(),
      effective_profile: t.json().notNull(),
      ceiling_id: t.text().notNull(),
      ceiling_version: t.bigInt().notNull(),
      gated_versions: t.json().notNull(),
      submitted_by: t.uuid().notNull(),
      submitted_at: t.timestamp().notNull().default({ fn: "now" }),
      approved_by: t.uuid(),
      approved_at: t.timestamp(),
      applied_at: t.timestamp(),
      last_error: t.text(),
    },
    primaryKey: ["app_id", "migration_id"],
  });
  // TODO(dsl-v2): json array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations ALTER COLUMN gated_versions SET DEFAULT '[]'::jsonb", reason: "column migrated_migrations.gated_versions requires exact default '[]'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint migrated_migrations.migrated_migrations_ceiling_version_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations ADD CONSTRAINT migrated_migrations_ceiling_version_check CHECK ((ceiling_version > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint migrated_migrations.migrated_migrations_status_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.migrated_migrations ADD CONSTRAINT migrated_migrations_status_check CHECK ((status = ANY (ARRAY['submitted'::text, 'pending_approval'::text, 'approved'::text, 'applied'::text, 'failed'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_currency_shape needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_currency_shape CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_fee_lte_gross needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_fee_lte_gross CHECK ((platform_fee <= gross_amount))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_fee_nonnegative needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_fee_nonnegative CHECK ((platform_fee >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_gross_nonnegative needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_gross_nonnegative CHECK ((gross_amount >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_net_matches_amounts needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_net_matches_amounts CHECK ((net_amount = (gross_amount - platform_fee)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payouts.control_payouts_net_nonnegative needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payouts ADD CONSTRAINT control_payouts_net_nonnegative CHECK ((net_amount >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint permission_tokens.permission_tokens_kind_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.permission_tokens ADD CONSTRAINT permission_tokens_kind_check CHECK ((kind = ANY (ARRAY['pat'::text, 'oauth_grant'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("platform_admin_roles", { schema: "zeroship" }).create({
    columns: {
      user_id: t.uuid().notNull(),
      role: t.text().notNull(),
      granted_at: t.timestamp().notNull().default({ fn: "now" }),
      granted_by: t.uuid(),
    },
    primaryKey: ["user_id"],
  });
  // TODO(dsl-v2): CHECK constraint platform_admin_roles.platform_admin_roles_role_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.platform_admin_roles ADD CONSTRAINT platform_admin_roles_role_check CHECK ((role = ANY (ARRAY['admin'::text, 'support'::text, 'billing'::text, 'readonly'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
