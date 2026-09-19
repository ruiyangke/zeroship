import { createFunction, raw, table } from "@zeroship/migrate";

// Placement eligibility: the Control-owned zone facts the workflow manager reads
// before it admits a placement. An execution zone is an operator-declared set
// of worker deployment units that share creator-side connectivity. An app
// belongs to exactly one zone, a worker instance belongs to the zone its join
// token claimed and Control resolved, and the manager places an app only on a
// worker of the same zone. Nothing a worker sends can change either fact: both
// live in rows no worker can write.
//
// Both zone columns are frozen - the app's by the trigger below, the
// instance's by the one that guards everything a join recorded
// (20260914000500_worker_join_bindings.ts). Moving an app between zones is a
// data migration of its creator storage, not a metadata edit. Placement relies
// on that: it reads the facts after taking its locks and again before commit,
// and a fact that could move between those two reads would reopen the window
// the second read exists to close.
export default {
  name: "placement_eligibility",
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

    // The manager reads an app's zone and terminal deletion, and an instance's
    // zone and lease; its identity and status columns are granted separately in
    // 20260911000000_workflow_coordination.ts. Column-scoped grants because
    // `SELECT` on the table would also open creator-owned columns Control wrote
    // into `apps`.
    raw({
      sql: "GRANT SELECT (execution_zone_id,deleted_at) ON zeroship.apps TO zeroship_workflow; "
        + "GRANT SELECT (execution_zone_id,expires_at) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "workflow placement reads Control-owned zone and lease facts without write authority",
    });
  },
};
