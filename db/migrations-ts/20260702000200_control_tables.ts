import { table, t, now, uuidV4 } from "@zeroship/migrate";

export default {
  name: "control_tables",
  schema() {
    table("app_audit", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        app_id: t.text(),
        creator_id: t.text(),
        actor_user_id: t.text(),
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
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["app_id", "key_name"],
    });
    table("app_members", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        user_id: t.text().notNull(),
        role: t.text().notNull(),
        added_at: t.timestamp().notNull().default(now()),
        added_by: t.text(),
      },
      primaryKey: ["app_id", "user_id"],
    });
    table("app_members", { schema: "zeroship" }).check("app_members_role_check").add({ expr: (col) => col("role").in(["owner", "editor", "viewer"]) });
    table("app_net_grants", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
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
        app_id: t.text().notNull(),
        client_id: t.text().notNull(),
        sector_identifier: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["app_id"],
    });
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
        app_id: t.text().notNull(),
        migration_id: t.uuid().notNull(),
        status: t.text().notNull(),
        request_body: t.json().notNull(),
        effective_profile: t.json().notNull(),
        ceiling_id: t.text().notNull(),
        ceiling_version: t.bigInt().notNull(),
        // The runtime descriptor this document set folds to, lowercase sha256 hex.
        // Validated at the migration service's door, so the spelling recorded here
        // is the one a manifest hash can be compared to with a plain `=`.
        descriptor_sha256: t.text().notNull(),
        // The engine's own `outcome.applied` for this request.
        applied_versions: t.json().notNull().default([]),
        submitted_by: t.text().notNull(),
        submitted_at: t.timestamp().notNull().default(now()),
        applied_at: t.timestamp(),
        last_error: t.text(),
      },
      primaryKey: ["app_id", "migration_id"],
    });
    table("app_schema_applies", { schema: "zeroship" }).check("app_schema_applies_ceiling_version_check").add({ expr: (col) => col("ceiling_version").gt(0) });
    table("app_schema_applies", { schema: "zeroship" }).check("app_schema_applies_status_check").add({ expr: (col) => col("status").in(["submitted", "applied", "failed"]) });
    table("app_scope_defs", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        scope_id: t.text().notNull(),
        label: t.text().notNull(),
        description: t.text(),
      },
      primaryKey: ["app_id", "scope_id"],
    });
    table("app_secrets", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        ciphertext: t.bytes().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["app_id", "key_name"],
    });
    table("app_usage", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        resource: t.text().notNull(),
        value: t.bigInt().notNull().default(0),
      },
      primaryKey: ["app_id", "resource"],
    });
    table("app_usage_history", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        period: t.text().notNull(),
        counters: t.json().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: null,
    });
    table("app_vars", { schema: "zeroship" }).create({
      columns: {
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        value: t.text().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["app_id", "key_name"],
    });
    table("apps", { schema: "zeroship" }).create({
      columns: {
        // A typed id, and deliberately WITHOUT a database default: a SQL-side
        // generator for `app_<base62>` would be a second minter beside
        // `AppId::mint`, and one producer per identifier is what makes a
        // derived name answerable. Every insert supplies the id.
        id: t.text().notNull(),
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
    // Must follow the create above: the recorder folds a pending schema after
    // each envelope, and a constraint naming a table the fold has not seen yet
    // fails the whole file with `table \`apps\` does not exist`.
    //
    // The character class is case-inclusive because `zeroship_core::typed_id`'s
    // BASE62 alphabet is `0..9A..Za..z` and case is significant in it; a
    // lower-only class would refuse most minted ids. Twenty-two characters is
    // base62 of 128 bits.
    table("apps", { schema: "zeroship" })
      .check("apps_id_shape")
      .add({ expr: (col) => col("id").regex("^app_[0-9A-Za-z]{22}$") });
    table("creator_account_history", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        creator_id: t.text().notNull(),
        stripe_account_id: t.text().notNull(),
        linked_at: t.timestamp().notNull().default(now()),
        unlinked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("creator_accounts", { schema: "zeroship" }).create({
      columns: {
        creator_id: t.text().notNull(),
        stripe_account_id: t.text().notNull(),
        onboarded_at: t.timestamp().notNull().default(now()),
        unlinked_at: t.timestamp(),
        charges_enabled: t.boolean().notNull().default(false),
        payouts_enabled: t.boolean().notNull().default(false),
        details_submitted: t.boolean().notNull().default(false),
      },
      primaryKey: ["creator_id"],
    });
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
        id: t.uuid().notNull().default(uuidV4()),
        creator_id: t.text().notNull(),
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
        owner_id: t.text().notNull(),
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
        user_id: t.text().notNull(),
        role: t.text().notNull(),
        granted_at: t.timestamp().notNull().default(now()),
        granted_by: t.text(),
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
        updated_by: t.text(),
      },
      primaryKey: ["id"],
    });
  },
};
