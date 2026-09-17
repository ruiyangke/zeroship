import { table } from "@zeroship/migrate";

// Nothing has ever read or written this table. A case-insensitive search over
// the whole tree finds no Rust reader, no Rust writer, and no fixture INSERT,
// in production or test, under any cfg or feature - only the CREATE, the grant
// to `zeroship_auth`, and the owner-registry entry that follows every table.
//
// It was a Hydra-era workaround: per-key `created_at` so JWK retirement could
// age keys individually instead of off the latest rotation timestamp.
//
// What replaced it is `zeroship.signing_keys`, the single OP signing registry
// keyed by `kid`. That registry drops the key-set name: a name for the whole
// set is stale vocabulary once one registry keyed by `kid` is the whole story.
export default {
  name: "drop_jwk_key_state",
  schema() {
    table("jwk_key_state", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
