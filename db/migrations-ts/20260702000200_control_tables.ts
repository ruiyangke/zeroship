import { table, t, now, uuidV4 } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  app_audit: ["actor_user_id"],
};

export default {
  name: "control_tables",
  schema() {
    table("app_audit", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        app_id: t.text(),
        actor_user_id: t.text(),
        action: t.text().required(),
        resource: t.text(),
        source_ip: t.inet(),
        detail: t.json(),
        occurred_at: t.timestamp().required().default(now()),
        // An audit row records what happened, and it must stay readable after
        // the organization it names is gone; it carries no foreign key on purpose.
        organization_id: t.text(),
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
        env_version: t.bigInt().required().default(0),
        workflows_enabled: t.boolean().required().default(false),
        manifest_json: t.text(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
        system: t.boolean().required().default(false),
        archived_at: t.timestamp(),
        // `project_id` is NULL only for a deleted app: deletion detaches the app
        // from its project while keeping `organization_id`, the column billing
        // attribution reads. `apps_live_app_has_project` holds that direction.
        project_id: t.text(),
        organization_id: t.text().required(),
        deleted_at: t.timestamp(),
        lifecycle_revision: t.bigInt().required().default(0),
        execution_zone_id: t.text().required().default("ezn_default000000000000000000"),
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
        id: t.uuid().required().default(uuidV4()),
        organization_id: t.text().required(),
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
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }
  },
};
