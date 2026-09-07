import { table } from "@zeroship/migrate";

// The refresh family moved onto `zeroship.sessions`, so nothing reads or writes
// this table any more. It is DROPPED rather than left in place: two stores for
// one credential is two answers to "is this token live", and the one nobody
// writes is the one an auditor would still find and believe.
//
// What the session row absorbed, column for column: `token_hash` and
// `hash_key_version` became `secret_hash` / `secret_key_version`;
// `replaced_by_token_hash` became `prev_secret_hash`, pointing the other way;
// `refresh_family_id` became the session's own id, so the family IS the row;
// `expires_at` and `family_absolute_expires_at` became `idle_expires_at` and
// `absolute_expires_at`; `sub`, `granted_scopes` and `family_granted_scopes`
// moved to the grant the session hangs off; and `rotated_at`, `revoked_at`,
// `idem_response_enc` and `idem_expires_at` kept their names and meanings.
//
// `zeroship.token_revocations` is NOT touched here. The marker that recalls an
// access token already in a client's hands is still written on every revoke;
// retiring that family is its own step, with its own replacement for what the
// marker does.
//
// The entry in policies/platform-table-owners.json STAYS. That file's entries
// outlive the tables they name, because the earlier migrations that create and
// constrain this one still run on a fresh database and the applier refuses
// fail-closed on any op targeting a table with no ownership entry.
export default {
  name: "drop_oauth_refresh_tokens",
  schema() {
    // CASCADE takes the foreign keys, the CHECK and the grants with it. The
    // auth role's privileges on this table are dropped by PostgreSQL along with
    // the object, so there is nothing left to revoke by name.
    table("oauth_refresh_tokens", { schema: "zeroship" }).drop({
      ifExists: true,
      cascade: true,
    });
  },
};
