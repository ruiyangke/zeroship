import { raw, revoke } from "@zeroship/migrate";

// The worker's database authority: none at all over the platform schema. It
// connects to the CREATOR database, opens each app under that app's runtime
// role, and reads app metadata from Control over HTTP rather than from a
// catalog table.
//
// ORDER IS LOAD-BEARING AND RUNS LATE ON PURPOSE. 20260702000900 grants the
// worker its original, wider privileges; this file must run after it for the
// deny below to be the final word. Filename order puts it after every file that
// grants anything in `zeroship`.
//
// No test asserts this end state - the argument above is reasoning, not a
// measurement. What IS measured is the worker's own boot gate:
// crates/zeroship-worker/src/db_posture.rs refuses to start against a login
// that can reach the platform schema at all.

export default {
  name: "worker_database_authority",
  schema() {
    // Start from zero effective authority over every current and future platform
    // relation. This schema-wide deny is the class boundary: it includes
    // device_grants and every other authorization-decision table without relying
    // on a hand-maintained sensitive-table list.
    raw({
      sql: "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker",
      reason: "the grant DSL has no ALL TABLES IN SCHEMA target",
    });
    raw({
      sql: "REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA zeroship FROM zeroship_worker",
      reason: "the worker must not advance platform-owned sequences",
    });
    // THESE TWO STATEMENTS STORE NOTHING. `ALTER DEFAULT PRIVILEGES
    // ... REVOKE` subtracts from the default privilege set; `zeroship_worker`
    // was never IN that set, so each is a no-op and `pg_default_acl` stays
    // empty. What actually denies the worker on a new platform table is
    // PostgreSQL's owner-only default: a fresh table has a null `relacl` and
    // nobody but the owner holds anything.
    //
    // They are kept because they make the INTENT explicit and would become
    // load-bearing the moment anything grants `zeroship_worker` a schema-wide
    // default. They are not a fence today. Do not cite them as one, and do not
    // read their presence as evidence that a later grant would be contradicted.
    raw({
      sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES FROM zeroship_worker",
      reason: "future platform tables must inherit the same deny-by-default boundary",
    });
    raw({
      sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON SEQUENCES FROM zeroship_worker",
      reason: "future platform sequences must inherit the same deny-by-default boundary",
    });
    revoke({
      privileges: ["create"],
      on: { kind: "schema", names: ["zeroship"] },
      from: ["zeroship_worker"],
    });
    // The zone split, as a privilege rather than as a deployment convention.
    // 20260702000900 granted USAGE alongside every other service; the worker
    // connects to the CREATOR database and reads app metadata from Control, so
    // it must not be able to resolve a platform relation even by name. This is
    // the privilege `crates/zeroship-worker/src/db_posture.rs` refuses to boot
    // without, so a deployment that hands the worker the platform DSN fails at
    // start instead of serving from the wrong zone.
    revoke({
      privileges: ["usage"],
      on: { kind: "schema", names: ["zeroship"] },
      from: ["zeroship_worker"],
    });
  },
};
