import { grant, now, raw, t, table } from "@zeroship/migrate";

// Normal deployment publication: a deploy is a command with a stable identity,
// and every lifecycle change Control makes is committed together with an
// intent that a Control driver later delivers to the workflow manager.
//
// `app_deploy_commands` is the immutable acceptance receipt of one deploy
// command, keyed by the client's command id. It binds the app, the actor, the
// operation, the normalized content type and the digest of the bytes Control
// actually consumed, and it holds the result returned to an exact retry. There
// is no UPDATE or DELETE grant: a receipt is written once. The actor is an
// attribution edge, so erasing the user clears it rather than blocking erasure.
//
// `app_lifecycle_intents` is Control's publication outbox, one row per app
// lifecycle revision (`apps.lifecycle_revision`). An activation carries the
// deployment and the exact input-free schedule registration; a disable carries
// neither. A pending activation is also a retention dependency: the deployment
// collector keeps its bundle until the manager's exact receipt is recorded.
//
// Typed-id columns take the bytewise collation here; the composite references
// to `app_deploys` follow in 20260914000200_deploy_publication_references.ts,
// because a composite key must match the referenced collation at every
// position when it is lowered against the live catalog.
export default {
  name: "deploy_publication",
  schema() {
    table("app_deploy_commands", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        app_id: t.text().required(),
        actor_id: t.text(),
        operation: t.text().required(),
        content_type: t.text().required(),
        archive_sha256: t.text().required(),
        deploy_id: t.text().required(),
        deploy_hash: t.text().required(),
        lifecycle_revision: t.bigInt(),
        result: t.text().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_deploy_commands", { schema: "zeroship" })
      .check("app_deploy_commands_id_shape")
      .add({ expr: (col) => col("id").regex("^dcm_[0-9a-z]{25}$") });
    table("app_deploy_commands", { schema: "zeroship" })
      .check("app_deploy_commands_actor_id_usr_shape")
      .add({ expr: (col) => col("actor_id").regex("^usr_[0-9a-z]{25}$") });
    table("app_deploy_commands", { schema: "zeroship" })
      .check("app_deploy_commands_operation_check")
      .add({ expr: (col) => col("operation").in(["deploy"]) });
    table("app_deploy_commands", { schema: "zeroship" })
      .check("app_deploy_commands_archive_sha256_check")
      .add({ expr: (col) => col("archive_sha256").regex("^[0-9a-f]{64}$") });
    table("app_deploy_commands", { schema: "zeroship" })
      .check("app_deploy_commands_lifecycle_revision_check")
      .add({ expr: (col) => col("lifecycle_revision").isNull().or(col("lifecycle_revision").gt(0)) });
    // Supports the app reference and the composite deployment reference added
    // in 20260914000200_deploy_publication_references.ts.
    table("app_deploy_commands", { schema: "zeroship" })
      .index("app_deploy_commands_deployment_idx")
      .add({ on: ["app_id", "deploy_id"] });

    table("app_lifecycle_intents", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        app_id: t.text().required(),
        revision: t.bigInt().required(),
        action: t.text().required(),
        deploy_id: t.text(),
        registration: t.text(),
        state: t.text().required(),
        receipt: t.text(),
        created_at: t.timestamp().required().default(now()),
        acknowledged_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .check("app_lifecycle_intents_id_shape")
      .add({ expr: (col) => col("id").regex("^lci_[0-9a-z]{25}$") });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .check("app_lifecycle_intents_revision_check")
      .add({ expr: (col) => col("revision").gt(0) });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .check("app_lifecycle_intents_action_check")
      .add({
        expr: (col) =>
          col("action")
            .eq("activate")
            .and(col("deploy_id").isNotNull(), col("registration").isNotNull())
            .or(col("action").eq("disable").and(col("deploy_id").isNull(), col("registration").isNull())),
      });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .check("app_lifecycle_intents_state_check")
      .add({
        expr: (col) =>
          col("state")
            .eq("pending")
            .and(col("receipt").isNull(), col("acknowledged_at").isNull())
            .or(col("state").eq("acknowledged").and(col("receipt").isNotNull(), col("acknowledged_at").isNotNull())),
      });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .unique("app_lifecycle_intents_app_id_revision_key")
      .add({ columns: ["app_id", "revision"] });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .index("app_lifecycle_intents_pending_idx")
      .add({ on: ["app_id", "revision"], where: (col) => col("state").eq("pending") });
    // Supports the composite deployment reference and the collector's lookup
    // of a pending activation for one deployment.
    table("app_lifecycle_intents", { schema: "zeroship" })
      .index("app_lifecycle_intents_deployment_idx")
      .add({ on: ["app_id", "deploy_id"] });

    // Typed-id domains and their copies compare bytewise; see
    // 20260831000001_sortable_entity_id_collations.ts.
    raw({
      sql:
        'ALTER TABLE "zeroship"."app_deploy_commands" ALTER COLUMN "id" TYPE text COLLATE "C", '
        + 'ALTER COLUMN "app_id" TYPE text COLLATE "C", ALTER COLUMN "actor_id" TYPE text COLLATE "C", '
        + 'ALTER COLUMN "deploy_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });
    raw({
      sql:
        'ALTER TABLE "zeroship"."app_lifecycle_intents" ALTER COLUMN "id" TYPE text COLLATE "C", '
        + 'ALTER COLUMN "app_id" TYPE text COLLATE "C", ALTER COLUMN "deploy_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison",
    });

    table("app_deploy_commands", { schema: "zeroship" })
      .foreignKey("app_deploy_commands_app_id_fkey")
      .add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    table("app_deploy_commands", { schema: "zeroship" })
      .foreignKey("app_deploy_commands_actor_id_fkey")
      .add({ columns: ["actor_id"], references: { table: "users", columns: ["id"] }, onDelete: "setNull" });
    table("app_lifecycle_intents", { schema: "zeroship" })
      .foreignKey("app_lifecycle_intents_app_id_fkey")
      .add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });

    grant({
      privileges: ["select", "insert"],
      on: { kind: "table", schema: "zeroship", names: ["app_deploy_commands"] },
      to: ["zeroship_control"],
    });
    grant({
      privileges: ["select", "insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["app_lifecycle_intents"] },
      to: ["zeroship_control"],
    });
  },
};
