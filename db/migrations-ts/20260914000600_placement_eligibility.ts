import { createFunction, raw, t, table } from "@zeroship/migrate";

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
    // ---- apps.execution_zone_id ---------------------------------------------
    // Control NAMES the zone when it creates an app: `Registry::create_app`
    // resolves the zone the creator asked for, or the deployment's one
    // declared zone when the creator named none, and refuses a deployment that
    // declares several without being told which. The creation path therefore
    // never relies on this default.
    //
    // The default names the deployment's single seeded zone
    // (20260914000450_execution_zones_default_zone.ts) and stays for the rows
    // written outside that path: harness scripts and test fixtures across
    // several crates insert an app row directly. The trigger below freezes
    // whatever value the row was created with, however it got there.
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

    // ---- the app's frozen zone ----------------------------------------------
    // Control holds UPDATE on this table for other columns (archive, delete),
    // and UPDATE is not column-selective in a grant, so the zone column is
    // frozen by trigger instead. A worker instance's zone is frozen by its own
    // table's trigger (20260914000500_worker_join_bindings.ts), which already
    // refuses every change to the identity a join recorded.
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
    // ---- the manager's read of the zone facts -------------------------------
    // Column grants only: the manager learns an app's zone and terminal
    // deletion, and an instance's zone and lease. Its id and status are already
    // granted by 20260911000000_workflow_coordination.ts. The lease is read as
    // a liveness hint, never as authority - Control refuses a lapsed instance
    // on every call it authenticates. The manager still cannot read creator
    // data, keys or addresses, and it cannot write any of these rows.
    raw({
      sql: "GRANT SELECT (execution_zone_id,deleted_at) ON zeroship.apps TO zeroship_workflow; "
        + "GRANT SELECT (execution_zone_id,expires_at) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "workflow placement reads Control-owned zone and lease facts without write authority",
    });
  },
};
