import { table, t, now, grant } from "@zeroship/migrate";

// The workflow scheduler store was created at RUNTIME, which no least-privilege
// deployment can do.
//
// crates/zeroship-workflow-scheduler/src/store.rs builds a provision_sql() that opens with
// `CREATE SCHEMA IF NOT EXISTS`, and crates/zeroship-control/src/cron/workflow_engine.rs
// called it on every tick. Postgres checks the database-level CREATE privilege
// BEFORE the IF NOT EXISTS existence short-circuit, so the statement fails even
// when the schema already exists.
//
// MEASURED 2026-08-11 against the live deployment, before this migration:
//   has_database_privilege('zeroship_control', current_database(), 'CREATE') = f
//   select count(*) from pg_namespace where nspname='workflow_scheduler' -> 0
//   control logged 56 `workflow_engine tick failed` ERRORs in 60 seconds, every
//   one of them SqlState(E42501) "permission denied for database postgres".
//
// So `env.workflows` could never schedule anything on a deployment that runs
// control under its own role. Granting database CREATE would fix the symptom and
// hand the control plane the right to create arbitrary schemas -- the exact
// privilege the role split exists to withhold. Platform tables belong to
// migrations, so the store is established here and the runtime now verifies
// (WorkflowSchedulerStore::ensure_ready) instead of creating.
//
// These live in `zeroship` rather than a `workflow_scheduler` schema of their
// own because the platform charter admits exactly two schemas. MEASURED: a first
// draft creating `workflow_scheduler` was REFUSED at lower time --
//   CROSS_SCHEMA op_index=1: op names schema "workflow_scheduler", which is not
//   in the permitted platform schema allow-list ["public", "zeroship"]
// -- and widening that allow-list would relax a real bound for a table that has
// no reason to sit outside the platform schema. The names carry the prefix the
// schema used to provide, matching the workflow_* tables already in `zeroship`.
//
// The store addresses scheduling state by run_id; id is the row identity.
const SCHEMA = "zeroship";

function zs(name) {
  return table(name, { schema: SCHEMA });
}

export default {
  name: "workflow_scheduler_store",
  schema() {
    zs("workflow_scheduler_timers").create({
      columns: {
        id: t.bigInt().notNull().identity(),
        run_id: t.text().notNull(),
        app_id: t.text().notNull(),
        wake_at: t.timestamp().notNull(),
        generation: t.bigInt().notNull().default(0),
        registered_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    zs("workflow_scheduler_timers").unique("workflow_scheduler_timers_natural_key").add({ columns: ["run_id"] });
    zs("workflow_scheduler_timers").index("workflow_scheduler_timers_due_idx").add({ on: ["wake_at"] });

    zs("workflow_scheduler_inflight").create({
      columns: {
        id: t.bigInt().notNull().identity(),
        run_id: t.text().notNull(),
        app_id: t.text().notNull(),
        deadline: t.timestamp().notNull(),
        dispatch_generation: t.bigInt().notNull(),
        dispatched_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    zs("workflow_scheduler_inflight").unique("workflow_scheduler_inflight_natural_key").add({ columns: ["run_id"] });
    zs("workflow_scheduler_inflight").index("workflow_scheduler_inflight_deadline_idx").add({ on: ["deadline"] });

    // Exactly the store's working set, measured by sweeping store.rs for the verbs
    // it issues against each table: INSERT/DELETE/SELECT on both, plus UPDATE on
    // inflight and the ON CONFLICT UPDATE arm of register_timer. TRUNCATE is used
    // only by clear_for_tests, which runs under a privileged DSN, so it is not
    // granted here.
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: {
        kind: "table",
        schema: SCHEMA,
        names: ["workflow_scheduler_timers", "workflow_scheduler_inflight"],
      },
      to: ["zeroship_control"],
    });
  },
};
