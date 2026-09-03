import { grant } from "@zeroship/migrate";

// `zeroship_gateway` could never set a token-family revocation marker, so
// signout and backchannel logout silently failed to revoke.
//
// crates/authz/src/wrapper_revocation.rs:41 revoke_family() is
//   INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after)
//   VALUES (...) ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = ...
// and the gateway calls it from two production sites,
// crates/gateway/src/backchannel_logout.rs:508 and browser_auth.rs:421.
// PostgreSQL requires UPDATE privilege to PLAN an ON CONFLICT DO UPDATE, so the
// statement failed on EVERY call, not only when a row already existed. Both call
// sites swallow the error into a log line, so signout still answered 204.
//
// MEASURED 2026-08-12 against a migrated database, as zeroship_gateway inside
// BEGIN ... ROLLBACK, one variable between the two statements:
//   INSERT ... VALUES (...)                          -> INSERT 0 1
//   INSERT ... VALUES (...) ON CONFLICT DO UPDATE    -> ERROR: permission denied
// and the privilege read:
//   SELECT=true  INSERT=true  UPDATE=false  DELETE=true
//
// deploy/compose/docker-compose.yml:411 runs the gateway as this exact role, so
// this was live in the shipped topology rather than latent.
//
// A NEW migration rather than an edit to 20260702000900: that one is applied and
// checksummed, so editing it in place would read as drift.
//
// Scoped to UPDATE on this one table. It is the minimum the statement needs, and
// it grants strictly less than the DELETE this role already holds on the same
// table -- UPDATE can only move a marker's timestamp, DELETE can remove the
// marker outright. Whether the gateway should hold DELETE at all is a separate
// question and is NOT settled here: no production DELETE was found in the crate,
// but a grep cannot prove absence, so that stays open rather than being quietly
// revoked alongside this fix.
export default {
  name: "gateway_token_revocations_update",
  schema() {
    grant({
      privileges: ["update"],
      on: { kind: "table", schema: "zeroship", names: ["token_revocations"] },
      to: ["zeroship_gateway"],
    });
  },
};
