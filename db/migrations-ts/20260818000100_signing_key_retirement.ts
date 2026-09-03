import { table, t } from "@zeroship/migrate";

// The signing-key trust horizon: a key leaves `retiring` for `retired` only
// once every token it ever issued has expired, which is what
// `max_issued_expires_at` records. Both halves were originally added by EDITING
// 20260702000300_auth_oauth_tables.ts in place (14b5ad800); that file was
// already applied to a deployed database, so the edit made the checksum guard
// refuse every later run. The delta is re-landed forward here.
//
// TWO STATEMENTS, ONE INTENT. On a fresh database the edited file produced the
// column and the four-value CHECK as part of CREATE TABLE. Reaching the same
// shape from an already-created table needs an ADD COLUMN and a
// replace-the-constraint pair, which is why this file is not a copy of the
// edit.
//
// WIDENING A CHECK IS TOTAL. `signing_keys_status_check` goes from
// {active, next, retiring} to {active, next, retiring, retired} - every row
// that satisfied the old predicate satisfies the new one, so the ADD cannot
// fail on existing data and no row is rewritten to a different status here.
// The drop is `ifExists` because a database that never had the constraint (one
// built from a future baseline) must not stall on the reversal.
export default {
  name: "signing_key_retirement",
  schema() {
    table("signing_keys", { schema: "zeroship" })
      .column("max_issued_expires_at")
      .add({ type: t.timestamp() });

    table("signing_keys", { schema: "zeroship" })
      .constraint("signing_keys_status_check")
      .drop({ ifExists: true });

    table("signing_keys", { schema: "zeroship" })
      .check("signing_keys_status_check")
      .add({ expr: (col) => col("status").in(["active", "next", "retiring", "retired"]) });
  },
};
