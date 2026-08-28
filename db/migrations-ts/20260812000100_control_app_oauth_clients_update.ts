import { grant } from "zero-migrate";

// `zeroship_control` could not provision a per-app OAuth client.
//
// crates/control/src/app_oauth_client.rs:591 (production; the file's
// #[cfg(test)] starts at 646) upserts the per-app extension row:
//   INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier)
//   VALUES ($1,$2,$3)
//   ON CONFLICT (app_id) DO UPDATE SET sector_identifier = ..., updated_at = NOW()
// and control held INSERT but not UPDATE on that table. PostgreSQL requires
// UPDATE to PLAN an ON CONFLICT DO UPDATE, so the statement failed on every
// call. The error is propagated (`.map_err(db_error)?`) inside the provisioning
// transaction, so the whole provision failed rather than degrading quietly --
// the opposite of the sibling defect on token_revocations, which was swallowed.
//
// MEASURED 2026-08-12 as zeroship_control, each arm in its own transaction so
// neither could mask the other, one variable between them:
//   INSERT ... VALUES (gen_random_uuid(), ...)
//     -> ERROR: violates foreign key constraint app_oauth_clients_app_id_fkey
//        (reached CONSTRAINT evaluation, so the INSERT privilege check passed)
//   INSERT ... VALUES (...) ON CONFLICT (app_id) DO UPDATE SET ...
//     -> ERROR: permission denied for table app_oauth_clients
// The FK error in the control arm is what makes it discriminating: it proves the
// statement got past privilege checking, which the ON CONFLICT arm never does.
//
// A NEW migration rather than an edit to 20260702000900: that one is applied and
// checksummed, so editing it in place would read as drift.
//
// Scoped to UPDATE on this one table -- the minimum the statement needs. The
// sibling INSERT into zeroship.app_scope_defs a few lines below is a plain
// INSERT with no ON CONFLICT, so it needs nothing and is deliberately untouched.
export default {
  name: "control_app_oauth_clients_update",
  schema() {
    grant({
      privileges: ["update"],
      on: { kind: "table", schema: "zeroship", names: ["app_oauth_clients"] },
      to: ["zeroship_control"],
    });
  },
};
