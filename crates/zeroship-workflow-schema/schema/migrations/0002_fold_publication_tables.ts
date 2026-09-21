import { table } from "../../../../packages/zero-migrate/dist/index.js";

// Retire the per-kind publication tables.
//
// Version 1 gives each published operation its own projection table, each
// deduplicated by a unique index over that kind's own tuple. The job id is now
// DERIVED from the work the job names (`publication_id` in
// crates/zeroship-core/src/workflow_jobs.rs), so `job_publications`' primary
// key IS the deduplication key: two transactions observing the same frontier,
// broadcast page or propagation page compute the same id, and the second
// insert collides instead of adding a second job. There is no per-kind tuple
// left for a per-kind table to hold.
//
// WHY A VERSION AND NOT AN EDIT TO `../schema.ts`. Version 1 is that file, so
// editing it is a legal way to change the journal - but only before anything
// has installed one. A journal already stamped at version 1 is refused at boot
// on a fingerprint it can never reach: the service applies exactly the versions
// BETWEEN its stamp and the target, and an edit to version 1 leaves that set
// empty. A version moves the fingerprint AND supplies the step that closes it,
// which is the signal `migrations/README.md` describes.
export function up(namespace) {
  for (const name of [
    "advance_publications",
    "fanout_publications",
    "propagation_publications",
  ]) {
    table(name, { schema: namespace }).drop();
  }
  // A drop declares no new identifier or column. The generator unions these
  // into the cumulative sets it checks the descriptor against, and that check
  // is one-way: a collection must have been declared, and one declared then
  // dropped is absent from the descriptor rather than unexpected in it.
  return { identifiers: new Set(), columns: new Set() };
}
