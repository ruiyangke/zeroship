import { table } from "@zeroship/migrate";

// Nothing has ever read or written this table. A case-insensitive search over
// the whole tree finds no Rust reader, no Rust writer, and no fixture INSERT,
// in production or test, under any cfg or feature - only the CREATE, the grant
// to `zeroship_auth`, and the owner-registry entry that follows every table.
//
// It was a Hydra-era workaround: per-key `created_at` so JWK retirement could
// age keys individually instead of off the latest rotation timestamp. That
// property was real, and if key retirement is ever revisited
// `docs/archive/reviews/auth-pre-merge-2026-05-28/round-07-crypto-findings.md`
// is the record of the bug the per-key column fixed.
//
// What replaced it is `zeroship.signing_keys`, the single OP signing registry
// keyed by `kid`. `docs/proposals/2026-06-30-auth-schema-redesign.md` planned
// that as a rename-and-expand of this table, and deliberately dropped
// `set_name` on the way: a key-set name is stale vocabulary once one registry
// keyed by `kid` is the whole story, and the final table should not be framed
// as a fix for a missing Hydra cron. The rename shipped as an ADDITION, so
// `signing_keys` has existed alongside this since; jwk_key_state is the residue
// nobody removed.
export default {
  name: "drop_jwk_key_state",
  schema() {
    table("jwk_key_state", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
