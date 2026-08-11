import { grant } from "@zeroship/migrate";

export const name = "control_audit_grants";

// `zeroship_control` could neither write the authorization audit trail nor run
// its own retention sweep.
//
// Two separate defects, one missing grant set.
//
// 1. THE WRITE. crates/authz/src/eval.rs:243 INSERTs into
//    zeroship.authz_decisions on every authorization decision, from
//    `enforce()`. Among the services in the compose stack, control is the only
//    caller of `enforce()`. The role it runs as held NO privilege on that table
//    at all: not INSERT, not SELECT, not DELETE. The insert failure is swallowed
//    (`if let Err(err) = ... { tracing::error!(...) }`) and `audit_decision`
//    returns unit, so under least privilege every decision would fail to record
//    with no functional symptom whatsoever.
//
// 2. THE SWEEP. crates/control/src/cron/audit_retention.rs is the sanctioned
//    deleter for both zeroship.app_audit and zeroship.authz_decisions
//    (sweep_all, the two delete_older_than calls). It needs DELETE, and also
//    SELECT: the statement is
//        DELETE FROM <table> WHERE occurred_at < NOW() - ...
//    and PostgreSQL requires SELECT on any column read in the WHERE clause.
//    Control held only INSERT on app_audit and nothing on authz_decisions.
//
// MEASURED 2026-08-11 against the live deployment, before this migration:
//   has_table_privilege('zeroship_control','zeroship.app_audit','INSERT') = true
//   has_table_privilege('zeroship_control','zeroship.app_audit','SELECT') = false
//   has_table_privilege('zeroship_control','zeroship.app_audit','DELETE') = false
//   has_table_privilege('zeroship_control','zeroship.authz_decisions', <any>) = false
// and, as zeroship_control against a live session:
//   insert into zeroship.authz_decisions ... -> ERROR: permission denied
//   insert into zeroship.app_audit ...       -> INSERT 0 1
//
// THIS DOES NOT WEAKEN THE APPEND-ONLY INVARIANT, which is worth stating
// because the grants alone read as though it might. Immutability on these
// tables is enforced by BEFORE DELETE / UPDATE / TRUNCATE triggers running
// <table>_block_tamper(), which refuse even the superuser. The trigger permits
// a DELETE only for a session that has opted in with
// `SET zeroship.audit_retention = 'on'`, which is exactly what the retention
// cron does on its own dedicated connection. UPDATE and TRUNCATE remain refused
// to everyone unconditionally. Granting DELETE here only lets the one sanctioned
// path reach a trigger it already satisfies; every other deletion attempt still
// raises `insufficient_privilege`.
//
// The shape mirrors the working reference already in the tree: `zeroship_auth`
// holds select/insert/delete on zeroship.audit_events and its own retention
// cron sets the same GUC. That pair works end to end; control was the same
// design with the grants left out.
//
// This is a NEW migration rather than an edit to 20260702000900 because that
// one has already been applied and is checksummed; editing it in place would
// read as drift rather than as a change.
//
// Scoped to control on purpose. `crates/migrated` also calls `enforce()`, but
// it is not a service in deploy/compose/docker-compose.yml and has no role of
// its own, so there is nothing to grant to yet. If it is ever deployed under a
// least-privilege role, it needs INSERT on authz_decisions for the same reason.
export function up() {
  grant({
    privileges: ["select", "delete"],
    on: { kind: "table", schema: "zeroship", names: ["app_audit"] },
    to: ["zeroship_control"],
  });
  grant({
    privileges: ["select", "insert", "delete"],
    on: { kind: "table", schema: "zeroship", names: ["authz_decisions"] },
    to: ["zeroship_control"],
  });
}
