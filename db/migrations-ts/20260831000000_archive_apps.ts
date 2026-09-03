import { raw, revoke, table, t } from "@zeroship/migrate";

// Archive is the app lifecycle boundary. It retains the row that owns billing
// attribution, migration history, the routable name, and database bindings.
// The control plane enforces the state at route publication and workflow
// admission. Deploys may still update the retained current artifact while an
// app is archived, but that artifact is neither routable nor schedulable until
// restore. Archive deliberately does not tear down the app's database:
// privileged role and schema lifecycle belongs to migrate-server.
//
// A nullable timestamp makes both transitions retry-safe while recording the
// first archive time. No index is needed yet: the route projection already
// scans the app registry as a set, while the worker version projection retains
// archived apps. Revisit a partial active-app index when registry scale makes
// that measured route scan material.
export default {
  name: "archive_apps",
  schema() {
    table("apps", { schema: "zeroship" })
      .column("archived_at")
      .add({ type: t.timestamp() });

    // The worker's final workflow claim is the last execution boundary. Its
    // database posture is column-deny-by-default, so grant only the new marker
    // that claim needs. Keeping this in the same migration prevents a window
    // where control can archive but workers cannot read the fence.
    raw({
      sql: "GRANT SELECT (archived_at) ON zeroship.apps TO zeroship_worker",
      reason: "workflow claims must reject archived apps at the final worker boundary",
    });

    // Hard delete has no control-plane verb anymore. Remove the process
    // capability as well as the code path so a future SQL injection cannot
    // revive the 25-cascade data-loss shape.
    revoke({
      privileges: ["delete"],
      on: { kind: "table", schema: "zeroship", names: ["apps"] },
      from: ["zeroship_control"],
    });
  },
};
