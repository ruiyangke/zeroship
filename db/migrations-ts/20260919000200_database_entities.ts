import { grant, now, raw, t, table } from "@zeroship/migrate";

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
// the second makes it its CLUSTER'S zone. Both keys, and the two the binding
// carries, are authored in 20260919000300_database_placement_keys.ts: a
// composite key is lowered position by position against a catalog snapshot
// taken before the migration runs, and the bytewise collation these columns
// need is applied here by a `raw` island that snapshot cannot see.
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
    // ---- datastores --------------------------------------------------------
    table("datastores", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        system_identifier: t.bigInt().required(),
        execution_zone_id: t.text().required(),
        status: t.text().required().default("pending"),
        last_error: t.text(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."datastores" ALTER COLUMN "id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql: 'ALTER TABLE "zeroship"."datastores" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "match the referenced execution zone identity collation",
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

    // ---- databases ---------------------------------------------------------
    table("databases", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        project_id: t.text().required(),
        execution_zone_id: t.text().required(),
        datastore_id: t.text().required(),
        name: t.text().required(),
        status: t.text().required().default("provisioning"),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
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
    table("databases", { schema: "zeroship" })
      .check("databases_id_shape")
      .add({ expr: (col) => col("id").regex("^dbs_[0-9a-z]{25}$") });
    table("databases", { schema: "zeroship" })
      .check("databases_status_check")
      .add({ expr: (col) => col("status").in(["provisioning", "active", "draining", "deleting"]) });
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

    // The indexes the two placement keys read from, declared rather than left to
    // the engine's emission: an emitted index is conditional on the live catalog,
    // which would make this plan a different length on a first apply than on a
    // re-apply.
    table("databases", { schema: "zeroship" })
      .index("databases_project_zone_fkey_idx")
      .add({ on: ["project_id", "execution_zone_id"] });
    table("databases", { schema: "zeroship" })
      .index("databases_placement_fkey_idx")
      .add({ on: ["datastore_id", "execution_zone_id"] });

    // ---- database_bindings -------------------------------------------------
    //
    // The edge carries its own id because the PostgreSQL role name is derived
    // from it - zs_bind_<binding>. A composite natural key would put two ids in
    // one identifier, and max_identifier_length truncates silently past 63 with
    // the id at the END of the name, so two bindings would collapse onto one
    // role rather than error, and revoking either would withdraw both.
    //
    // It carries no label: the creator's local name for a database rides in the
    // manifest, so Control never treats a creator-chosen name as an identifier
    // and two apps may call one database different things. It carries no role
    // name: which roles exist is answerable from the cluster catalog, which is
    // authoritative, and a stored copy could only be wrong.
    table("database_bindings", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        app_id: t.text().required(),
        database_id: t.text().required(),
        project_id: t.text().required(),
        capability: t.text().required(),
        status: t.text().required().default("pending"),
        // Control declares; a per-cluster reconciler converges. Nothing spans
        // the control database and a tenant cluster, so the row and its roles
        // cannot be written in one transaction and `observed_generation` is how
        // a reader tells a declared binding from a live one.
        generation: t.int().required().default(1),
        observed_generation: t.int().required().default(0),
        last_error: t.text(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
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

    // The indexes the two edges read from, for the reason above.
    table("database_bindings", { schema: "zeroship" })
      .index("database_bindings_app_project_fkey_idx")
      .add({ on: ["app_id", "project_id"] });
    table("database_bindings", { schema: "zeroship" })
      .index("database_bindings_database_project_fkey_idx")
      .add({ on: ["database_id", "project_id"] });

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
