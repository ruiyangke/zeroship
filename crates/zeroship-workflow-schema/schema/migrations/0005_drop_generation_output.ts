import { table } from "../../../../packages/zero-migrate/dist/index.js";

// A run's final output is a payload object, so the generation row keeps no
// inline slot for one.
//
// Every value a run returns is staged through the payload transport and named by
// `output_ref`, whatever its size: the runner references it rather than deciding
// per value, so there is no size at which a run output lands in a column
// instead. A run that returns nothing stages nothing, which a null `output_ref`
// already says.
//
// A column nothing can write is worse than an absent one. The closed set in
// `journal_payload_columns_are_a_closed_set`
// (crates/zeroship-workflow-server/tests/platform_schema.rs) reads the installed
// schema and matches on a column's NAME as well as its type, so an `output`
// column left empty still counts as a place creator payload lives. Dropping it
// is what makes that set smaller.
//
// No foreign key and no index names the column: the `generations` keys are over
// `app_id`, `deploy_id`, `run_id` and `generation`. SQLite therefore drops it in
// place, with no table rebuild for this host to refuse.
export function up(namespace) {
  table("generations", { schema: namespace }).column("output").drop();
  // A drop declares no new identifier and no new column. The generator unions
  // these into the cumulative sets it checks the descriptor against, and a
  // column declared by an earlier version and dropped here is absent from the
  // descriptor rather than unexpected in it.
  return { identifiers: new Set(), columns: new Set() };
}
