import { t, table } from "@zeroship/migrate";

// `apps.lifecycle_revision` is the app's lifecycle revision high-water mark.
// Deploy activation, archive disable and restore activation each take the next
// value under the app row lock, a value is never reused, and the workflow
// manager refuses an unknown revision below one it has accepted - so Control
// delivers an app's lifecycle intents strictly in revision order.
//
// A separate file from the intent tables: altering `apps` declares the table
// to the migration recorder, which then validates every foreign key naming
// `apps` in the same file against that partial declaration.
export default {
  name: "app_lifecycle_revision",
  schema() {
    table("apps", { schema: "zeroship" })
      .column("lifecycle_revision")
      .add({ type: t.bigInt().notNull().default(0) });
    table("apps", { schema: "zeroship" })
      .check("apps_lifecycle_revision_check")
      .add({ expr: (col) => col("lifecycle_revision").ge(0) });
  },
};
