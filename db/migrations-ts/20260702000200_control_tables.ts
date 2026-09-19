import { table, t, now, uuidV4 } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  app_audit: ["actor_user_id"],
  app_schema_applies: ["submitted_by"],
};

export default {
  name: "control_tables",
  schema() {
    table("app_audit", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        app_id: t.text(),
        actor_user_id: t.text(),
        action: t.text().notNull(),
        resource: t.text(),
        source_ip: t.inet(),
        detail: t.json(),
        occurred_at: t.timestamp().notNull().default(now()),
        // An audit row records what happened, and it must stay readable after
        // the organization it names is gone; it carries no foreign key on purpose.
        organization_id: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_env_expose", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_env_expose", { schema: "zeroship" }).unique("app_env_expose_natural_key").add({ columns: ["app_id", "key_name"] });
    table("app_oauth_clients", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        client_id: t.text().notNull(),
        sector_identifier: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
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
    // (`catalog::admit_schema`), which compares a `.zship`'s
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
        id: t.bigInt().notNull().identity(),
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
        submitted_by: t.text(),
        submitted_at: t.timestamp().notNull().default(now()),
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
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        scope_id: t.text().notNull(),
        label: t.text().notNull(),
        description: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_scope_defs", { schema: "zeroship" }).unique("app_scope_defs_natural_key").add({ columns: ["app_id", "scope_id"] });
    table("app_secrets", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        ciphertext: t.bytes().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_secrets", { schema: "zeroship" }).unique("app_secrets_natural_key").add({ columns: ["app_id", "key_name"] });
    table("app_usage", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        resource: t.text().notNull(),
        value: t.bigInt().notNull().default(0),
      },
      primaryKey: ["id"],
    });
    table("app_usage", { schema: "zeroship" }).unique("app_usage_natural_key").add({ columns: ["app_id", "resource"] });
    table("app_usage_history", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        period: t.text().notNull(),
        counters: t.json().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_vars", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        key_name: t.text().notNull(),
        value: t.text().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
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
        id: t.text().notNull(),
        name: t.text().notNull(),
        plan_id: t.text().notNull().default("free"),
        deploy_hash: t.text(),
        env_version: t.bigInt().notNull().default(0),
        workflows_enabled: t.boolean().notNull().default(false),
        manifest_json: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
        system: t.boolean().notNull().default(false),
        archived_at: t.timestamp(),
        // `project_id` is NULL only for a deleted app: deletion detaches the app
        // from its project while keeping `organization_id`, the column billing
        // attribution reads. `apps_live_app_has_project` holds that direction.
        project_id: t.text(),
        organization_id: t.text().notNull(),
        deleted_at: t.timestamp(),
        lifecycle_revision: t.bigInt().notNull().default(0),
        execution_zone_id: t.text().notNull().default("ezn_default000000000000000000"),
      },
      primaryKey: ["id"],
    });
    // Must follow the create above: the recorder folds a pending schema after
    // each envelope, and a constraint naming a table the fold has not seen yet
    // fails the whole file with `table \`apps\` does not exist`.
    //
    // The character class is lowercase-only, matching `zeroship-id`'s BASE36
    // alphabet `0-9a-z`: the alphabet is single-case on purpose, because an app
    // id is also a schema name, a DNS label and a policy scope segment, each of
    // which folds case, and a mixed-case body would give one id two spellings.
    // Twenty-five characters is the minimum fixed width that holds 128 bits
    // (36^24 < 2^128 <= 36^25).
    table("apps", { schema: "zeroship" })
      .check("apps_id_shape")
      .add({ expr: (col) => col("id").regex("^app_[0-9a-z]{25}$") });
    table("apps", { schema: "zeroship" })
      .check("apps_project_id_shape")
      .add({ expr: (col) => col("project_id").regex("^prj_[0-9a-z]{25}$") });
    table("apps", { schema: "zeroship" })
      .check("apps_organization_id_shape")
      .add({ expr: (col) => col("organization_id").regex("^org_[0-9a-z]{25}$") });
    // Deletion is a refinement of archive: every fence that already excludes an
    // archived app keeps excluding a deleted one with no new predicate, and a
    // live app always names a project.
    table("apps", { schema: "zeroship" })
      .check("apps_deleted_app_is_archived")
      .add({ expr: (col) => col("deleted_at").isNull().or(col("archived_at").isNotNull()) });
    table("apps", { schema: "zeroship" })
      .check("apps_live_app_has_project")
      .add({ expr: (col) => col("project_id").isNotNull().or(col("deleted_at").isNotNull()) });
    table("apps", { schema: "zeroship" })
      .check("apps_lifecycle_revision_check")
      .add({ expr: (col) => col("lifecycle_revision").ge(0) });
    table("payouts", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        organization_id: t.text().notNull(),
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
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }
  },
};
