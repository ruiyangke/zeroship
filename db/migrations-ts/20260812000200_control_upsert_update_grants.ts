import { grant } from "@zeroship/migrate";

// Two more production upserts `zeroship_control` could not execute. Same class
// as the token_revocations/gateway fix (20260812000000) and the
// app_oauth_clients fix (20260812000100): the statement uses
// ON CONFLICT ... DO UPDATE, PostgreSQL requires UPDATE privilege to PLAN that,
// and the role was granted only select/insert/delete.
//
// 1. zeroship.app_vars -- crates/control/src/env_store.rs:236, the env-var write
//    path behind `zeroship var set`. The upsert sits inside a CTE whose outer
//    statement bumps apps.env_version, so the whole creator-facing operation
//    failed under least privilege.
// 2. zeroship.token_revocations -- crates/control/src/oauth_grants_handlers.rs:223,
//    which revokes a token family when an OAuth grant is deleted.
//
// MEASURED 2026-08-12 as zeroship_control, each in its own transaction:
//   INSERT INTO zeroship.app_vars(...) ON CONFLICT (app_id,key_name) DO UPDATE ...
//     -> ERROR: permission denied for table app_vars
//   INSERT INTO zeroship.token_revocations(...) ON CONFLICT (client_id,sub) DO UPDATE ...
//     -> ERROR: permission denied for table token_revocations
//
// FOUND BY SWEEPING, not by stumbling: every (role, table) pair with INSERT but
// not UPDATE, crossed against every production ON CONFLICT DO UPDATE target.
// That direction is sound because the grant list is authoritative and finite.
//
// Scoped to UPDATE on these two tables -- the minimum each statement needs.
// control already holds UPDATE on zeroship.apps, which the env_store CTE also
// writes, so that half was never the blocker.
export default {
  name: "control_upsert_update_grants",
  schema() {
    grant({
      privileges: ["update"],
      on: { kind: "table", schema: "zeroship", names: ["app_vars", "token_revocations"] },
      to: ["zeroship_control"],
    });
  },
};
