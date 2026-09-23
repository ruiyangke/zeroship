import { table } from "../../../../packages/zero-migrate/dist/index.js";

// A run's input is a payload object, so the generation row keeps no inline slot
// for one.
//
// Every value a run is started from is staged through the payload transport and
// named by `input_ref`, whatever its size: the host that accepted the value
// references it rather than deciding per value, so there is no size at which a
// run input lands in a column instead. A run started from nothing stages
// nothing, which a null `input_ref` already says.
//
// A column nothing can write is worse than an absent one. The closed set in
// `journal_payload_columns_are_a_closed_set`
// (crates/zeroship-workflow-server/tests/platform_schema.rs) reads the installed
// schema and matches on a column's NAME as well as its type, so an `input`
// column left empty still counts as a place creator payload lives. Dropping it
// is what empties that set.
//
// No foreign key and no index names the column: the `generations` keys are over
// `app_id`, `deploy_id`, `run_id` and `generation`. SQLite therefore drops it in
// place, with no table rebuild for this host to refuse.
export function up(namespace) {
  table("generations", { schema: namespace }).column("input").drop();
  // A drop declares no new identifier and no new column. The generator unions
  // these into the cumulative sets it checks the descriptor against, and a
  // column declared by an earlier version and dropped here is absent from the
  // descriptor rather than unexpected in it.
  return { identifiers: new Set(), columns: new Set() };
}
