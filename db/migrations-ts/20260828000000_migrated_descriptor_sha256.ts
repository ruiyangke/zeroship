import { table, t } from "@zeroship/migrate";

// The deploy schema precondition needs ONE fact control can read on its own
// connection: which runtime descriptor the app's schema currently corresponds
// to. `zeroship.migrated_migrations` is the only migration record control holds
// a grant on (`20260702000900_grants.ts` gives zeroship_control select/insert/
// update), and the engine's own journal lives in a per-app schema no other role
// can reach - so the hash is recorded here, beside the request that applied it.
//
// NULLABLE, AND THE NULL IS FAIL-CLOSED. Rows written before this column
// existed carry NULL, and the deploy predicate compares with `=`, which is NULL
// for a NULL row - never true. An app whose newest applied row predates this
// column therefore cannot deploy until it migrates once more, which stamps a
// hash. That is the direction to fail in: the alternative (`IS NOT DISTINCT
// FROM`) would let a manifest with NO descriptor past a NULL row, which is the
// "omit one JSON key" bypass the second arm of the predicate exists to close.
//
// ONE ROW PER APPLY REQUEST, not per applied migration - see the note on
// `descriptor_sha256` in `crates/zeroship-migrated/src/apply.rs`. A re-run that
// applies nothing still inserts a row, and that is load-bearing: it is the only
// way an app whose descriptor bytes changed without a schema change (an engine
// upgrade, a codegen fix) can ever deploy again.
export default {
  name: "migrated_descriptor_sha256",
  schema() {
    table("migrated_migrations", { schema: "zeroship" })
      .column("descriptor_sha256")
      .add({ type: t.text() });
  },
};
