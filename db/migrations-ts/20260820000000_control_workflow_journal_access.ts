import { raw } from "zero-migrate";

// Control could not read ANY per-app workflow journal. `app_<uuid>` is created
// AUTHORIZATION zeroship_workflow_owner (crates/migrated/src/provisioning.rs,
// workflow_journal_schema_sql) and 20260818000200 granted that role to
// zeroship_worker only, so every control cron that probes a journal with
// `to_regclass('app_<uuid>.__zeroship_workflow_runs')` raised
// `permission denied for schema app_<uuid>` - measured on the deployed database
// at ~4 errors/second across workflow_engine and workflow_signal_fanout.
//
// WHY MEMBERSHIP AND NOT A NARROWER GRANT. The narrow option is USAGE on the
// schema plus SELECT/INSERT/UPDATE/DELETE on the five journal tables, and it is
// not enough for two independent reasons:
//
//   1. Control CREATES journal tables. workflow_instance_api.rs (start run) and
//      cron/workflow_schedules.rs both call PgStore::provision, whose first
//      statement is `SET ROLE zeroship_workflow_owner`
//      (crates/plugin-workflow/src/store/pg.rs, set_workflow_journal_owner_role_sql).
//      SET ROLE is gated on MEMBERSHIP; no combination of table privileges
//      substitutes for it.
//   2. The journal schemas are per-app and created after this file runs. A
//      static migration cannot name `app_<uuid>` for an app that does not exist
//      yet, so it cannot grant USAGE on it at all. Membership in the owning role
//      is the only privilege a migration can express here that covers apps
//      created later.
//
// WHAT THIS DOES NOT WIDEN. zeroship_workflow_owner owns nothing but the
// `app_<uuid>` journal schemas and their five `__zeroship_workflow_*` tables. It
// is NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION
// NOBYPASSRLS and holds no CREATE on `zeroship` (20260818000200). An app's
// CREATOR DATA lives in the bare `<uuid>` schema, which is a different schema
// with a different owner (see the doc comment on
// migrated::provisioning::workflow_journal_schema_name), so this grant gives
// control no reach into creator tables.
export default {
  name: "control_workflow_journal_access",
  schema() {
    raw({
      sql: "GRANT zeroship_workflow_owner TO zeroship_control",
      reason:
        "control provisions and sweeps per-app workflow journals; SET ROLE needs membership, not privileges",
    });
  },
};
