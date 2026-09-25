import { grant, raw } from "@zeroship/migrate";

// The reach `zeroship_workflow` holds outside its own schema: USAGE on the two
// platform schemas it reads across, column-scoped SELECT on the Control tables
// its coordinator and placement reads project, and DML on the shared assertion
// replay store it settles service assertions against. The role is created in
// 20260911000000_workflow_coordination.ts, so this file sorts after it.
export default {
  name: "workflow_platform_grants",
  schema() {
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship", "service_authn"] }, to: ["zeroship_workflow"] });
    raw({
      sql: "GRANT SELECT (id,status,public_key) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "the coordinator verifies enrolled worker identity without customer-data access",
    });
    // No grant for the policy inputs. `apps(plan_id, workflows_enabled,
    // archived_at)` and the whole of `zeroship.plans` are what the policy
    // ledger reads, and it reads them over Control's app-facts endpoint now, so
    // the workflow role holds nothing on either.
    //
    // Nothing on `zeroship.apps.deploy_hash` or on `zeroship.app_deploys`
    // either, for the same reason. The deployment catalog is Control's: a
    // management command names its deployment on the wire and the manager does
    // not decide which one is current (`Coordinator::manage` in
    // crates/zeroship-workflow-manager/src/coordinator/management.rs), and the
    // holds it takes against that catalog go through Control's queue endpoint,
    // under Control's credential and inside Control's row lock. A capability
    // this process has no reader for is one it must not hold.
    //
    // What `apps` is granted for is placement, and this file grants the
    // identity half: `ControlEligibility::app`
    // (crates/zeroship-workflow-manager/src/eligibility.rs) filters on `id`,
    // and 20260914000600_placement_eligibility.ts grants the zone and deletion
    // facts it projects.
    raw({
      sql: "GRANT SELECT (id) ON zeroship.apps TO zeroship_workflow",
      reason: "placement resolves an app's zone facts by id without creator data or catalog authority",
    });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "service_authn", names: ["service_assertion_replay"] },
      to: ["zeroship_workflow"],
    });
  },
};
