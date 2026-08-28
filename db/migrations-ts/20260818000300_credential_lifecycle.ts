import { table, grant } from "zero-migrate";

// Credential lifecycle enforcement: the two lookups every disable / anonymize /
// delete path performs, and the one privilege the linking path was missing.
// Originally landed by EDITING 20260702000600_constraints_indexes_fks.ts and
// 20260702000900_grants.ts in place (290c85e0a). Both were already journalled
// by a deployed database, so the edits made the checksum guard refuse. Re-landed
// forward here.
//
// WHY AN INSERT GRANT APPEARS TO WIDEN AND DOES NOT. 20260702000900 grants
// zeroship_auth select+update on `app_user_identities`; the edit added insert,
// so auth can create the identity row it then enforces lifecycle on rather than
// failing on first link. `app_session_anchors` is untouched here: the edit only
// SPLIT it out of a shared name list so the insert would not reach it, and the
// split is a no-op once the added privilege is granted to the one table alone.
export default {
  name: "credential_lifecycle",
  schema() {
    // Reverse lookup from the platform user to every app identity that must be
    // disabled with it. Without it this is a sequential scan per lifecycle event.
    table("app_user_identities", { schema: "zeroship" })
      .index("app_user_identities_global_user_id_idx")
      .add({ on: ["global_user_id"] });

    // PARTIAL BY DESIGN. Every authenticating path asks "is this user still
    // allowed to authenticate", and the answer is no for the small minority of
    // rows carrying any lifecycle timestamp. Indexing only those rows keeps the
    // index proportional to the exceptions rather than to the user table.
    table("users", { schema: "zeroship" })
      .index("auth_users_non_authenticating_idx")
      .add({
        on: ["id"],
        where: (col) =>
          col("disabled_at")
            .isNotNull()
            .or(
              col("anonymized_at").isNotNull(),
              col("deletion_requested_at").isNotNull(),
              col("deletion_scheduled_for").isNotNull(),
            ),
      });

    grant({
      privileges: ["insert"],
      on: { kind: "table", schema: "zeroship", names: ["app_user_identities"] },
      to: ["zeroship_auth"],
    });
  },
};
