import { t, table } from "../../../../packages/zero-migrate/dist/index.js";

// The successor a generation's close handed its work to.
//
// A run that continues as new closes and names the run that carries on. That
// name is PLATFORM data: the journal minted it, creator code never sees it in
// the return value, and nothing about it is the creator's to choose. Carrying
// it in `output` would put it in the one column that otherwise holds arbitrary
// creator JSON, where a creator returning the same shape is indistinguishable
// from a continuation. A typed column of its own is what tells them apart.
//
// Nullable, because most closes hand their work to nobody. A close that DOES
// produce a successor sets it, whatever produced the successor, so the column
// is already the answer when a retry or a cron occurrence starts naming one.
export function up(namespace) {
  table("generations", { schema: namespace })
    .column("continued_as_new_run_id")
    .add({ type: t.text() });
  // A column add declares no new identifier. The name is returned so the
  // owned-name binder refuses an identifier that collides with it.
  return { identifiers: new Set(), columns: new Set(["continued_as_new_run_id"]) };
}
