import { createFunction, raw, table } from "@zeroship/migrate";

// An app's execution zone. An execution zone is an operator-declared set of
// worker deployment units that share creator-side connectivity. An app belongs
// to exactly one zone, a worker instance belongs to the zone its join token
// claimed and Control resolved, and a worker serves only the apps of its own
// zone. Nothing a worker sends can change either fact: both live in rows no
// worker can write.
//
// Both zone columns are frozen - the app's by the trigger below, the
// instance's by the one that guards everything a join recorded
// (20260914000500_worker_join_bindings.ts). Moving an app between zones is a
// data migration of its creator storage, not a metadata edit. The workflow
// manager relies on that: a queue scope records its app's zone once, and a
// zone claim compares that copy with the claiming instance's zone, so a zone
// that could move would leave the copy naming a zone the app had left.
export default {
  name: "app_execution_zones",
  schema() {
    // ---- apps_execution_zone_fk ---------------------------------------------
    // An app's zone is fixed when the app is created; this pins it to a
    // declared zone and the trigger below refuses any later change.
    table("apps", { schema: "zeroship" })
      .foreignKey("apps_execution_zone_fk")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."apps" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison with execution_zones.id",
    });

    // ---- the app's frozen zone ----------------------------------------------
    // Control holds UPDATE on this table for other columns (archive, delete),
    // and its table-wide UPDATE grant also covers the zone column, so the zone
    // is frozen by trigger instead. A worker instance's zone is frozen by its
    // own table's trigger (20260914000500_worker_join_bindings.ts), which
    // already refuses every change to the identity a join recorded.
    createFunction({
      schema: "zeroship",
      name: "apps_reject_execution_zone_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.execution_zone_id IS DISTINCT FROM OLD.execution_zone_id THEN\n"
        + "    RAISE EXCEPTION 'an app''s execution zone is fixed when the app is created'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });
    table("apps", { schema: "zeroship" })
      .trigger("apps_frozen_execution_zone")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "apps_reject_execution_zone_change",
      });

    // The workflow service authorizes a worker instance by its frozen zone and
    // refuses it once its lease lapses. The `id` it filters by, the status and
    // the key are granted in 20260911000050_workflow_platform_grants.ts, so its
    // reach over this table is the union of the two files and neither one
    // alone. Column-scoped because `SELECT` on the table would also open the
    // address and join columns that service never reads. It holds nothing on
    // `zeroship.apps`: an app's zone reaches it in Control's app facts and in
    // the queue scope Control's lifecycle messages create.
    raw({
      sql: "GRANT SELECT (execution_zone_id,expires_at) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "the workflow service authorizes a worker by its frozen zone and live lease without write authority",
    });
  },
};
