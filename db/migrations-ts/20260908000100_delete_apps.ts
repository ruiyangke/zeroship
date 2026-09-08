import { table, t } from "@zeroship/migrate";

// The terminal end of the app lifecycle, and the last step of the account
// closure funnel: erase account -> dissolve organization -> delete project ->
// DELETE APP. Every earlier refusal names the next step, and until this file
// the last one named a route that did not exist.
//
// ---- Why the row survives its own deletion --------------------------------
//
// `zeroship.apps` is the billing subject. Two of its children say so in
// opposite directions and both are load-bearing:
//
//   invoice_lines.app_id     ON DELETE RESTRICT - a finalized line may not lose
//                            the app it prices, so PostgreSQL refuses the row
//                            delete outright once an invoice exists.
//   usage_aggregates.app_id  ON DELETE CASCADE - the input to the unbilled-usage
//                            predicate that `dissolve` and the erasure preflight
//                            both read. A row delete does not refuse here; it
//                            silently takes the evidence with it.
//
// A hard delete is therefore either impossible or destructive depending only on
// whether the reconciler has run yet, which is the worst of both. Deletion is a
// marker instead: nothing cascades, the ledger is untouched, and the app stops
// being reachable. `20260831000000_archive_apps.ts` already revoked DELETE on
// this table from `zeroship_control`, and that revocation stands - the verb
// added here is an UPDATE.
//
// ---- Why the project edge comes off ---------------------------------------
//
// `apps_project_ownership_fkey` is ON DELETE RESTRICT, so a retained row would
// pin its project forever and the funnel would still not terminate - it would
// merely refuse one step later, at a constraint name instead of a sentence.
// Deletion detaches the app from its project and keeps `organization_id`, which
// is the column billing attribution actually reads. The organization survives
// the app; the project does not have to.
//
// `project_id` therefore loses NOT NULL, and the check below is what stops that
// from widening into "an app with no project". A NULL there is spellable only
// for a row that has been deleted.
//
// ---- Why deletion is a refinement of archive, not a sibling ----------------
//
// Every fence that already excludes an archived app - route publication, the
// worker's final workflow claim, workflow admission, the schedule and signal
// sweeps - keeps excluding a deleted one, with no new predicate anywhere,
// because `apps_deleted_app_is_archived` makes "deleted" imply "archived" in
// the catalog rather than by convention. That is the whole reason archive is a
// precondition of delete and not just a courtesy.
export default {
  name: "delete_apps",
  schema() {
    table("apps", { schema: "zeroship" })
      .column("deleted_at")
      .add({ type: t.timestamp() });

    table("apps", { schema: "zeroship" }).column("project_id").dropNotNull();

    // Deleted implies archived. Written as a constraint rather than left to the
    // control plane so the ordering survives any future writer of this table:
    // the fences downstream read `archived_at`, and they are only complete for
    // deleted apps while this holds.
    table("apps", { schema: "zeroship" })
      .check("apps_deleted_app_is_archived")
      .add({
        expr: (col) => col("deleted_at").isNull().or(col("archived_at").isNotNull()),
      });

    // A live app always names a project. Detachment is legal only in the one
    // direction deletion needs it.
    table("apps", { schema: "zeroship" })
      .check("apps_live_app_has_project")
      .add({
        expr: (col) => col("project_id").isNotNull().or(col("deleted_at").isNotNull()),
      });
  },
};
