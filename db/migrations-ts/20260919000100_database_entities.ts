import { createFunction, grant, now, raw, t, table } from "@zeroship/migrate";

// The app/database decoupling's three entities, from
// docs/proposals/2026-08-28-app-database-decoupling.md.
//
// An app id is a tenant. It is not a schema name, not a role name, not an
// encryption salt and not a publication key. Today it is all five by string
// identity, which is what makes a database that outlives its app - or one that
// two apps share - unrepresentable. These tables give that identity its own
// home.
//
// WHAT THE FOREIGN KEYS ARE FOR, because they are the design rather than
// bookkeeping. `database_bindings` is the N:M edge, and a binding may only join
// an app and a database IN THE SAME PROJECT. That predicate is carried by two
// COMPOSITE foreign keys over a shared `project_id` - `(app_id, project_id)`
// consumed by `apps(id, project_id)` and `(database_id, project_id)` consumed by
// `databases(id, project_id)` - so the two sides agree on every write to either,
// rather than being checked once at issuance by something that could forget. It
// is the mechanism `apps_project_ownership_fkey` already uses for app ownership.
//
// AND WHY THE BINDING CARRIES NO ZONE. A project sits in exactly one execution
// zone, so "same project" already implies "same zone" and a zone pair on the
// binding would be a second copy of a fact the project holds. Co-location is not
// an extra predicate the edge enforces; it falls out of ownership. `databases`
// still carries the zone, under two composite keys that between them say the
// whole placement rule: the first makes a database's zone its PROJECT'S zone,
// the second makes it its CLUSTER'S zone.
//
// THE ZONE MOVES TO THE PROJECT. `apps.execution_zone_id`
// (20260914000600_placement_eligibility.ts) landed before anything above the app
// needed a zone. With a project-owned database it is the project that has to
// carry it, or "same project" stops implying "can share" and every sharing
// surface has to explain a second rule. The app keeps its copy under a composite
// foreign key so the two cannot disagree, because `instance_serves_app` joins on
// it and the workflow manager holds a column grant on it; both keep working
// untouched.
//
// `datastores` IS KEYED ON THE CLUSTER'S OWN IDENTITY, not on a chosen name.
// `system_identifier` comes from `pg_control_system()` and is stable for the life
// of a cluster, identical from every database in it, and carried forward by a
// promoted physical replica. Keying on it makes registration idempotent: two
// services configured against one cluster converge on one row, and a mistyped
// DSN either fails to connect or reaches a different cluster where it becomes a
// visibly new row rather than a silent duplicate. There is no DSN and no secret
// reference on the row - a cluster's credential stays in the config of the
// service that holds it and never reaches Control.
export default {
  name: "database_entities",
  schema() {
    // ---- projects gains the zone ------------------------------------------
    table("projects", { schema: "zeroship" })
      .column("execution_zone_id")
      .add({ type: t.text().notNull().default("ezn_default000000000000000000") });
    table("projects", { schema: "zeroship" })
      .foreignKey("projects_execution_zone_fkey")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."projects" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison with execution_zones.id",
    });
    table("projects", { schema: "zeroship" })
      .unique("projects_zone_identity_key")
      .add({ columns: ["id", "execution_zone_id"] });

    // Frozen for the same reason an app's zone is: placement reads the fact
    // after taking its locks and again before commit, and a fact that could move
    // between those two reads would reopen the window the second read closes.
    createFunction({
      schema: "zeroship",
      name: "projects_reject_execution_zone_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.execution_zone_id IS DISTINCT FROM OLD.execution_zone_id THEN\n"
        + "    RAISE EXCEPTION 'a project''s execution zone is fixed when the project is created'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });
    table("projects", { schema: "zeroship" })
      .trigger("projects_frozen_execution_zone")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "projects_reject_execution_zone_change",
      });

    // ---- apps: the identity keys the bindings consume ----------------------
    //
    // `apps` carries only `apps_name_key` today, and its composite ownership key
    // points OUTWARD at projects with nothing pointing in. PostgreSQL requires a
    // real unique constraint on referenced columns, so without this the binding
    // foreign key cannot create.
    table("apps", { schema: "zeroship" })
      .unique("apps_project_identity_key")
      .add({ columns: ["id", "project_id"] });
    // And the app's zone copy cannot disagree with its project's.
    table("apps", { schema: "zeroship" })
      .foreignKey("apps_project_zone_fkey")
      .add({
        columns: ["project_id", "execution_zone_id"],
        references: { table: "projects", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });

    // ---- datastores --------------------------------------------------------
    table("datastores", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        system_identifier: t.bigInt().notNull(),
        execution_zone_id: t.text().notNull(),
        status: t.text().notNull().default("pending"),
        last_error: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("datastores", { schema: "zeroship" })
      .check("datastores_id_shape")
      .add({ expr: (col) => col("id").regex("^dst_[0-9a-z]{25}$") });
    // `pending` is not yet bootstrapped; placement admits `active` only, so a
    // cluster that is unreachable or half-bootstrapped is never chosen.
    // `draining` is how a cluster leaves rotation without a deploy.
    table("datastores", { schema: "zeroship" })
      .check("datastores_status_check")
      .add({ expr: (col) => col("status").in(["pending", "active", "draining", "retired", "failed"]) });
    table("datastores", { schema: "zeroship" })
      .unique("datastores_system_identifier_key")
      .add({ columns: ["system_identifier"] });
    table("datastores", { schema: "zeroship" })
      .unique("datastores_zone_identity_key")
      .add({ columns: ["id", "execution_zone_id"] });
    table("datastores", { schema: "zeroship" })
      .foreignKey("datastores_execution_zone_fkey")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."datastores" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."datastores" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "match the referenced execution zone identity collation",
    });

    // ---- databases ---------------------------------------------------------
    table("databases", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        project_id: t.text().notNull(),
        execution_zone_id: t.text().notNull(),
        datastore_id: t.text().notNull(),
        name: t.text().notNull(),
        // A ROLE-NAME INPUT, NOT A RECORD OF THE SCHEMA. It answers which
        // zs_bind_<binding>_e<epoch> Control should compose, and nothing else.
        // Its authority is the epoch row on the cluster, written inside the
        // transaction that mints the epoch's roles; this is a projection kept so
        // a binding can be composed without a cross-zone read. A stale copy
        // composes a role name that does not exist, SET LOCAL ROLE fails, and
        // the caller re-resolves - fail-closed and self-correcting. Reading it
        // as a description of shape rebuilds the deleted deploy gate under
        // another name.
        schema_epoch: t.int().notNull().default(0),
        status: t.text().notNull().default("provisioning"),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("databases", { schema: "zeroship" })
      .check("databases_id_shape")
      .add({ expr: (col) => col("id").regex("^dbs_[0-9a-z]{25}$") });
    table("databases", { schema: "zeroship" })
      .check("databases_status_check")
      .add({ expr: (col) => col("status").in(["provisioning", "active", "draining", "deleting"]) });
    table("databases", { schema: "zeroship" })
      .check("databases_schema_epoch_nonnegative")
      .add({ expr: (col) => col("schema_epoch").ge(0) });
    // Display text the CLI dereferences locally. A database is addressed by its
    // id on every wire; there is no (project, name) resolution anywhere.
    table("databases", { schema: "zeroship" })
      .unique("databases_project_name_key")
      .add({ columns: ["project_id", "name"] });
    table("databases", { schema: "zeroship" })
      .unique("databases_project_identity_key")
      .add({ columns: ["id", "project_id"] });
    table("databases", { schema: "zeroship" })
      .foreignKey("databases_project_fkey")
      .add({
        columns: ["project_id"],
        references: { table: "projects", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    // The zone is its PROJECT'S zone ...
    table("databases", { schema: "zeroship" })
      .foreignKey("databases_project_zone_fkey")
      .add({
        columns: ["project_id", "execution_zone_id"],
        references: { table: "projects", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    // ... and its CLUSTER'S zone. Together: a database is placed on a cluster in
    // its project's zone, structurally, with no trigger and nothing to forget.
    table("databases", { schema: "zeroship" })
      .foreignKey("databases_placement_fkey")
      .add({
        columns: ["datastore_id", "execution_zone_id"],
        references: { table: "datastores", columns: ["id", "execution_zone_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."databases" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."databases" ALTER COLUMN "project_id" TYPE text COLLATE "C"',
      reason: "match the referenced project identity collation",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."databases" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "match the referenced execution zone identity collation",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."databases" ALTER COLUMN "datastore_id" TYPE text COLLATE "C"',
      reason: "match the referenced datastore identity collation",
    });

    // ---- database_bindings -------------------------------------------------
    //
    // The edge carries its own id because the PostgreSQL role name is derived
    // from it - zs_bind_<binding>_e<epoch>. A composite natural key would put
    // two ids in one identifier, and max_identifier_length truncates silently
    // past 63 with the epoch at the END of the name, so two epochs would
    // collapse onto one role rather than error.
    //
    // It carries no label: the creator's local name for a database rides in the
    // manifest, so Control never treats a creator-chosen name as an identifier
    // and two apps may call one database different things. It carries no epoch:
    // which role names exist for a binding is answerable from the cluster
    // catalog, which is authoritative, and a stored copy could only be wrong.
    table("database_bindings", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        app_id: t.text().notNull(),
        database_id: t.text().notNull(),
        project_id: t.text().notNull(),
        capability: t.text().notNull(),
        status: t.text().notNull().default("pending"),
        // Control declares; a per-cluster reconciler converges. Nothing spans
        // the control database and a tenant cluster, so the row and its roles
        // cannot be written in one transaction and `observed_generation` is how
        // a reader tells a declared binding from a live one.
        generation: t.int().notNull().default(1),
        observed_generation: t.int().notNull().default(0),
        last_error: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("database_bindings", { schema: "zeroship" })
      .check("database_bindings_id_shape")
      .add({ expr: (col) => col("id").regex("^bnd_[0-9a-z]{25}$") });
    table("database_bindings", { schema: "zeroship" })
      .check("database_bindings_capability_check")
      .add({ expr: (col) => col("capability").in(["readwrite", "readonly"]) });
    table("database_bindings", { schema: "zeroship" })
      .check("database_bindings_status_check")
      .add({ expr: (col) => col("status").in(["pending", "active", "revoking", "revoked"]) });
    table("database_bindings", { schema: "zeroship" })
      .check("database_bindings_generation_order")
      .add({ expr: (col) => col("observed_generation").le(col("generation")) });
    table("database_bindings", { schema: "zeroship" })
      .unique("database_bindings_natural_key")
      .add({ columns: ["app_id", "database_id"] });
    // THE TWO EDGES. Both agree on project_id, so an app can bind only a
    // database in its own project, on every write to either side.
    table("database_bindings", { schema: "zeroship" })
      .foreignKey("database_bindings_app_project_fkey")
      .add({
        columns: ["app_id", "project_id"],
        references: { table: "apps", columns: ["id", "project_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    table("database_bindings", { schema: "zeroship" })
      .foreignKey("database_bindings_database_project_fkey")
      .add({
        columns: ["database_id", "project_id"],
        references: { table: "databases", columns: ["id", "project_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."database_bindings" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."database_bindings" ALTER COLUMN "app_id" TYPE text COLLATE "C"',
      reason: "match the referenced app identity collation",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."database_bindings" ALTER COLUMN "database_id" TYPE text COLLATE "C"',
      reason: "match the referenced database identity collation",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."database_bindings" ALTER COLUMN "project_id" TYPE text COLLATE "C"',
      reason: "match the referenced project identity collation",
    });

    // ---- grants ------------------------------------------------------------
    //
    // Control creates and deletes databases and bindings, and owns every
    // lifecycle transition on a datastore's `status`. It does NOT insert
    // datastores: a cluster registers itself through the service that holds its
    // credential, which is the only proof that it exists and is reachable.
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "zeroship", names: ["databases", "database_bindings"] },
      to: ["zeroship_control"],
    });
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["datastores"] },
      to: ["zeroship_control"],
    });
  },
};
