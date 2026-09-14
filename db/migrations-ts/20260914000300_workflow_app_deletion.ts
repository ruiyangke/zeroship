import { raw } from "@zeroship/migrate";

// The workflow manager abandons, rather than closes, the recovery
// responsibility of an app whose terminal deletion Control recorded. It reads
// the deletion marker through a column grant beside the archive and deployment
// columns it already reads, and gains no other access to the app row.
export default {
  name: "workflow_app_deletion",
  schema() {
    raw({
      sql: "GRANT SELECT (deleted_at) ON zeroship.apps TO zeroship_workflow",
      reason: "the workflow manager abandons responsibility for apps Control deleted",
    });
  },
};
