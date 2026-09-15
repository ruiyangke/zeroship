import { createFunction, raw, t, table } from "@zeroship/migrate";

// Placement eligibility: the Control-owned zone facts the workflow manager reads
// before it admits a placement. An execution zone is an operator-declared set
// of worker deployment units that share creator-side connectivity. An app
// belongs to exactly one zone, a worker instance belongs to the zone of the
// enroller Control verified when it enrolled, and the manager places an app
// only on a worker of the same zone. Nothing a worker sends can change either
// fact: both live here, in rows no worker can write.
//
// Both zone columns are frozen. Moving an app between zones is a data
// migration of its creator storage, not a metadata edit, and an enroller's
// zone is part of the deployment unit's identity. Placement relies on that:
// it reads the facts after taking its locks and again before commit, and a
// fact that could move between those two reads would reopen the window the
// second read exists to close.
export default {
  name: "placement_eligibility",
  schema() {
    // ---- apps.execution_zone_id ---------------------------------------------
    // The default names the deployment's single seeded zone
    // (20260914000050_execution_zones_default_zone.ts). A single-zone
    // deployment has exactly one choice, so an app created without naming a
    // zone lands in it. A deployment with more than one zone must name the
    // zone when it creates the app; the frozen trigger below then keeps it.
    table("apps", { schema: "zeroship" })
      .column("execution_zone_id")
      .add({ type: t.text().notNull().default("ezn_default000000000000000000") });
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

    // ---- worker_enrollers.execution_zone_id ---------------------------------
    // 20260914000000_execution_zones_and_worker_enrollers.ts left this column
    // as unenforced intent because a same-file foreign key to the freshly
    // collated execution_zones.id is refused. That collation now exists, so
    // the reference is enforced here.
    table("worker_enrollers", { schema: "zeroship" })
      .foreignKey("worker_enrollers_execution_zone_fk")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
      });

    // ---- frozen zone facts ---------------------------------------------------
    // Control holds UPDATE on both tables for other columns (archive, delete,
    // revoke), and UPDATE is not column-selective in a grant, so the zone
    // columns are frozen by trigger instead.
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
    createFunction({
      schema: "zeroship",
      name: "worker_enrollers_reject_execution_zone_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.execution_zone_id IS DISTINCT FROM OLD.execution_zone_id THEN\n"
        + "    RAISE EXCEPTION 'an enroller''s execution zone is fixed when it is provisioned'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });
    table("worker_enrollers", { schema: "zeroship" })
      .trigger("worker_enrollers_frozen_execution_zone")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "worker_enrollers_reject_execution_zone_change",
      });

    // ---- the manager's read of the zone facts -------------------------------
    // Column grants only: the manager learns an app's zone and terminal
    // deletion, an instance's enroller, and an enroller's zone and status.
    // It still cannot read creator data, keys or addresses, and it cannot
    // write any of these rows.
    raw({
      sql: "GRANT SELECT (execution_zone_id,deleted_at) ON zeroship.apps TO zeroship_workflow; "
        + "GRANT SELECT (enroller_id) ON zeroship.worker_instances TO zeroship_workflow; "
        + "GRANT SELECT (id,execution_zone_id,status) ON zeroship.worker_enrollers TO zeroship_workflow",
      reason: "workflow placement reads Control-owned zone and enrollment facts without write authority",
    });
  },
};
