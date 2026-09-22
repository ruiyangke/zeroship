import { t, table } from "../../../../packages/zero-migrate/dist/index.js";

// A revision for the run's REPLAY JOURNAL. `frontier_revision` is not one.
//
// `frontier_revision` authorizes one dispatch and is pinned for that
// authorization's whole lifetime by construction: the advance job is published
// at a revision, `publication_id` hashes that revision into an immutable
// operation, `tasks::assign` stamps it on the task it mints inside the same
// transaction that consumes the job, and `authorize_task` then refuses every
// later claim whose task disagrees with the run. A quantity that has to change
// WHILE a dispatch is outstanding is a different quantity from the one that
// authorizes it.
//
// Rewriting the journal is exactly such a change. A durable wait settled
// against the database clock rewrites its step row with nothing published: no
// frontier transition covers it, so `frontier_revision` stands still while the
// journal a replay is handed moves. `journal_revision` counts those rewrites.
//
// `runs` carries the live value; `tasks` carries the value its dispatch was
// minted against, which is what lets a reclaimed dispatch's strike be counted
// against the journal state it died on rather than one the journal has since
// left behind.
export function up(namespace) {
  for (const name of ["runs", "tasks"]) {
    table(name, { schema: namespace })
      .column("journal_revision")
      .add({ type: t.bigInt().required().default(1) });
  }
  // A column add declares no new identifier. The name is returned so the
  // owned-name binder refuses an identifier that collides with it.
  return { identifiers: new Set(), columns: new Set(["journal_revision"]) };
}
