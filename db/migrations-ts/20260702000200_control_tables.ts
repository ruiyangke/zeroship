import { table, t, now, uuidV4 } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  app_audit: ["creator_id", "actor_user_id"],
  app_members: ["user_id", "added_by"],
  app_net_grants: ["granted_by"],
  app_schema_applies: ["submitted_by"],
  creator_account_history: ["creator_id"],
  creator_accounts: ["creator_id"],
  net_policy_catalog: ["updated_by"],
  payouts: ["creator_id"],
  permission_tokens: ["owner_id"],
  platform_admin_roles: ["user_id", "granted_by"],
  platform_policies: ["updated_by"],
};

export default {
  name: "control_tables",
  schema() {
    table("app_audit", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        app_id: t.text(),
        creator_id: t.text(),
        actor_user_id: t.text(),
        actor_token_id: t.uuid(),
        action: t.text().required(),
        resource: t.text(),
        source_ip: t.inet(),
        detail: t.json(),
        occurred_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_env_expose", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        key_name: t.text().required(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_env_expose", { schema: "zeroship" }).unique("app_env_expose_natural_key").add({ columns: ["app_id", "key_name"] });
    table("app_members", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        user_id: t.text().required(),
        role: t.text().required(),
        added_at: t.timestamp().required().default(now()),
        added_by: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_members", { schema: "zeroship" }).unique("app_members_natural_key").add({ columns: ["app_id", "user_id"] });
    table("app_members", { schema: "zeroship" }).check("app_members_role_check").add({ expr: (col) => col("role").in(["owner", "editor", "viewer"]) });
    table("app_net_grants", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        host: t.text().required(),
        port: t.int().required(),
        granted_by: t.text().required(),
        granted_at: t.timestamp().required().default(now()),
        note: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_net_grants", { schema: "zeroship" }).unique("app_net_grants_natural_key").add({ columns: ["app_id", "host", "port"] });
    table("app_net_grants", { schema: "zeroship" }).check("app_net_grants_port_check").add({ expr: (col) => col("port").ge(1).and(col("port").le(65535)) });
    table("app_oauth_clients", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        client_id: t.text().required(),
        sector_identifier: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_oauth_clients", { schema: "zeroship" }).unique("app_oauth_clients_natural_key").add({ columns: ["app_id"] });
    // ONE ROW PER SCHEMA-APPLY REQUEST the migration service accepted. It is the
    // platform's own record of what an app's schema corresponds to, and it exists
    // BECAUSE the engine journal is not usable as one: that journal now lives in
    // the app's own schema, which the app's migrator role OWNS, and an owner can
    // DROP it. So the platform keeps its own row and never treats the tenant's
    // journal as a trust anchor.
    //
    // The reader is the control plane's deploy precondition
    // (`Registry::set_deploy_with_manifest`), which compares a `.zship`'s
    // `runtime_descriptor.hash` against the NEWEST applied row's
    // `descriptor_sha256`. `zeroship_control` holds the grant
    // (`20260702000900_grants.ts`) and no other service reads it.
    //
    // A ROW IS WRITTEN PER REQUEST, EVEN WHEN NOTHING APPLIED, and that is a
    // requirement rather than an accident of where the insert sits: an engine
    // upgrade that changes descriptor bytes without changing any schema would
    // otherwise halt every app's next deploy forever. `applied_versions` is what
    // the engine reported as applied for that request, so a re-run that applied
    // nothing is visible as `[]` rather than being indistinguishable from one
    // that advanced the schema.
    table("app_schema_applies", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        migration_id: t.uuid().required(),
        status: t.text().required(),
        request_body: t.json().required(),
        effective_profile: t.json().required(),
        ceiling_id: t.text().required(),
        ceiling_version: t.bigInt().required(),
        // The runtime descriptor this document set folds to, lowercase sha256 hex.
        // Validated at the migration service's door, so the spelling recorded here
        // is the one a manifest hash can be compared to with a plain `=`.
        descriptor_sha256: t.text().required(),
        // The engine's own `outcome.applied` for this request.
        applied_versions: t.json().required().default([]),
        submitted_by: t.text().required(),
        submitted_at: t.timestamp().required().default(now()),
        applied_at: t.timestamp(),
        last_error: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_schema_applies", { schema: "zeroship" }).unique("app_schema_applies_natural_key").add({ columns: ["app_id", "migration_id"] });
    table("app_schema_applies", { schema: "zeroship" }).check("app_schema_applies_ceiling_version_check").add({ expr: (col) => col("ceiling_version").gt(0) });
    table("app_schema_applies", { schema: "zeroship" }).check("app_schema_applies_status_check").add({ expr: (col) => col("status").in(["submitted", "applied", "failed"]) });
    table("app_scope_defs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        scope_id: t.text().required(),
        label: t.text().required(),
        description: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_scope_defs", { schema: "zeroship" }).unique("app_scope_defs_natural_key").add({ columns: ["app_id", "scope_id"] });
    table("app_secrets", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        key_name: t.text().required(),
        ciphertext: t.bytes().required(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_secrets", { schema: "zeroship" }).unique("app_secrets_natural_key").add({ columns: ["app_id", "key_name"] });
    table("app_usage", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        resource: t.text().required(),
        value: t.bigInt().required().default(0),
      },
      primaryKey: ["id"],
    });
    table("app_usage", { schema: "zeroship" }).unique("app_usage_natural_key").add({ columns: ["app_id", "resource"] });
    table("app_usage_history", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        period: t.text().required(),
        counters: t.json().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_vars", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        key_name: t.text().required(),
        value: t.text().required(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_vars", { schema: "zeroship" }).unique("app_vars_natural_key").add({ columns: ["app_id", "key_name"] });
    table("apps", { schema: "zeroship" }).create({
      columns: {
        // A typed id, and deliberately WITHOUT a database default: a SQL-side
        // generator for `app_<base36>` would be a second minter beside
        // `AppId::mint`, and one producer per identifier is what makes a
        // derived name answerable. Every insert supplies the id.
        id: t.text().required(),
        name: t.text().required(),
        plan_id: t.text().required().default("free"),
        deploy_hash: t.text(),
        api_key: t.text().required(),
        api_key_hash: t.text().required().default(""),
        env_version: t.bigInt().required().default(0),
        suspended: t.boolean().required().default(false),
        audit_locked: t.boolean().required().default(false),
        workflows_enabled: t.boolean().required().default(false),
        manifest_json: t.text(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
        system: t.boolean().required().default(false),
      },
      primaryKey: ["id"],
    });
    // Must follow the create above: the recorder folds a pending schema after
    // each envelope, and a constraint naming a table the fold has not seen yet
    // fails the whole file with `table \`apps\` does not exist`.
    //
    // The character class is case-inclusive because `zeroship_core::typed_id`'s
    // BASE36 alphabet is `0..9A..Za..z` and case is significant in it; a
    // lower-only class would refuse most minted ids. Twenty-two characters is
    // base36 of 128 bits.
    table("apps", { schema: "zeroship" })
      .check("apps_id_shape")
      .add({ expr: (col) => col("id").regex("^app_[0-9a-z]{25}$") });
    table("creator_account_history", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        creator_id: t.text().required(),
        stripe_account_id: t.text().required(),
        linked_at: t.timestamp().required().default(now()),
        unlinked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("creator_accounts", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        stripe_account_id: t.text().required(),
        onboarded_at: t.timestamp().required().default(now()),
        unlinked_at: t.timestamp(),
        charges_enabled: t.boolean().required().default(false),
        payouts_enabled: t.boolean().required().default(false),
        details_submitted: t.boolean().required().default(false),
      },
      primaryKey: ["id"],
    });
    table("creator_accounts", { schema: "zeroship" }).unique("creator_accounts_natural_key").add({ columns: ["creator_id"] });
    table("net_policy_catalog", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        key: t.text().required(),
        value_json: t.json().required(),
        updated_by: t.text(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("net_policy_catalog", { schema: "zeroship" }).unique("net_policy_catalog_natural_key").add({ columns: ["key"] });
    table("payouts", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        creator_id: t.text().required(),
        event_id: t.text().required(),
        event_type: t.text().required(),
        gross_amount: t.bigInt().required(),
        platform_fee: t.bigInt().required(),
        net_amount: t.bigInt().required(),
        currency: t.text().required(),
        occurred_at: t.timestamp().required(),
        payload_hash: t.bytes(),
        created_at: t.timestamp().required().default(now()),
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
        id: t.uuid().required(),
        owner_id: t.text().required(),
        kind: t.text().required(),
        client_id: t.text(),
        name: t.text().required(),
        policies: t.json().required(),
        policy_hash: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        expires_at: t.timestamp(),
        revoked_at: t.timestamp(),
        last_used_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("permission_tokens", { schema: "zeroship" }).check("permission_tokens_kind_check").add({ expr: (col) => col("kind").in(["pat", "oauth_grant"]) });
    table("platform_admin_roles", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        user_id: t.text().required(),
        role: t.text().required(),
        granted_at: t.timestamp().required().default(now()),
        granted_by: t.text(),
      },
      primaryKey: ["id"],
    });
    table("platform_admin_roles", { schema: "zeroship" }).unique("platform_admin_roles_natural_key").add({ columns: ["user_id"] });
    table("platform_admin_roles", { schema: "zeroship" }).check("platform_admin_roles_role_check").add({ expr: (col) => col("role").in(["admin", "support", "billing", "readonly"]) });
    table("platform_policies", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        cedar_source: t.text().required(),
        enabled: t.boolean().required().default(true),
        updated_at: t.timestamp().required().default(now()),
        updated_by: t.text(),
      },
      primaryKey: ["id"],
    });
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }
  },
};
