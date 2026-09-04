import { grant } from "@zeroship/migrate";

// `zeroship_control` held NO privilege of any kind on
// zeroship.connect_checkout_failures, and two production paths use it.
//
// FOUND BY RUNNING, not by reading. Booting control under its real role rather
// than as the superuser produces, within seconds:
//   ERROR control::cron::billing_notify
//         "control billing-notify tick failed" error="database: db error"
// The same binary, same flags, same database, with only the DSN changed to the
// superuser logs no error at all. One variable, opposite outcomes.
//
// THE READ. crates/zeroship-control/src/cron/billing_notify.rs:374 selects from this
// table as one of seven sources it unions for notification transitions. The
// SELECT is not optional or guarded, so the ENTIRE billing-notify tick fails
// every time it runs: no billing notification of ANY kind is produced on a
// least-privilege deployment, including the six kinds sourced from tables the
// role can read perfectly well.
//
// THE WRITE. crates/zeroship-control/src/stripe_store.rs:420 inserts into it. So a
// Connect checkout failure could never be recorded either.
//
// MEASURED 2026-08-11 against the live deployment, before this migration:
//   has_table_privilege('zeroship_control','zeroship.connect_checkout_failures', <any>) = false
// and, as zeroship_control over a real password-authenticated connection:
//   select count(*) from zeroship.connect_checkout_failures
//     -> ERROR: permission denied for table connect_checkout_failures
//   select count(*) from zeroship.invoices        (same role, granted table)
//     -> 0
//
// THE ERROR TEXT IS PART OF THE DEFECT and is left as a separate concern.
// "database: db error" names neither the table nor the privilege, which is why
// this survived: the one operator-visible symptom carries no information, and
// the tick failing is otherwise silent. Nothing retries into a louder state.
//
// This is a NEW migration rather than an edit to 20260702000900 because that one
// has already been applied and is checksummed.
export default {
  name: "control_connect_failures_grant",
  schema() {
    grant({
      privileges: ["select", "insert"],
      on: { kind: "table", schema: "zeroship", names: ["connect_checkout_failures"] },
      to: ["zeroship_control"],
    });
  },
};
