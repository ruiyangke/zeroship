import { grant } from "@zeroship/migrate";

// `zeroship_auth` could never run its own retention sweep.
//
// crates/auth/src/cron/token_sweep.rs:125-128 DELETEs from
// zeroship.token_revocations and surfaces the row count as
// `token_revocations_deleted`. The role it runs as was granted only
// select/insert/update on that table (20260702000900_grants.ts, the grant
// covering oauth_grants + oauth_clients + token_revocations), so every sweep
// failed and the table grew without bound.
//
// MEASURED 2026-08-11 against the live deployment, before this migration:
//   has_table_privilege('zeroship_auth','zeroship.token_revocations','SELECT') = true
//   has_table_privilege(...,'INSERT') = true
//   has_table_privilege(...,'DELETE') = false
//
// This is a NEW migration rather than an edit to 20260702000900 because that
// one has already been applied and is checksummed; editing it in place would
// read as drift rather than as a change.
//
// Scoped to this one table on purpose. The existing auth grant lists
// token_revocations alongside oauth_grants and oauth_clients, so adding
// "delete" there would also hand auth the right to delete OAuth grants and
// clients, which nothing asks for. `zeroship_gateway` already holds
// select/insert/delete on this exact table, so auth is not gaining a privilege
// its peer lacks.
export default {
  name: "auth_token_revocations_delete",
  schema() {
    grant({
      privileges: ["delete"],
      on: { kind: "table", schema: "zeroship", names: ["token_revocations"] },
      to: ["zeroship_auth"],
    });
  },
};
