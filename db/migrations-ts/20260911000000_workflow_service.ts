import { grant, raw, revoke, role, schema } from "@zeroship/migrate";
import { workflowSchema } from "../../crates/zeroship-workflow/schema/schema.ts";
import { workflowPlatformPolicy } from "../../crates/zeroship-workflow/schema/platform-policy.ts";
import fingerprints from "../../crates/zeroship-workflow/schema/fingerprints.json" with { type: "json" };

export default {
  name: "workflow_service",
  schema() {
    role("zeroship_workflow_migrator").create({ login: false });
    // Deployment provisions the login's credential independently of migrations.
    role("zeroship_workflow").create({ login: true, setSearchPath: ["workflow", "pg_catalog"] });
    schema("workflow").create({ authorization: "zeroship_workflow_migrator" });
    const tables = workflowSchema("workflow");
    for (const name of tables) {
      raw({
        sql: `ALTER TABLE workflow."${name}" OWNER TO zeroship_workflow_migrator`,
        reason: "journal ownership belongs to a migration-only role",
      });
    }
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["workflow"] }, to: ["zeroship_workflow"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "workflow", names: ["schema_version"] }, to: ["zeroship_workflow"] });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "workflow", names: tables.filter(name => name !== "schema_version") },
      to: ["zeroship_workflow"],
    });
    revoke({ privileges: ["all"], on: { kind: "schema", names: ["workflow"] }, from: ["PUBLIC", "zeroship_control", "zeroship_worker", "zeroship_gateway", "zeroship_app"] });
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["service_authn"] }, to: ["zeroship_workflow"] });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "service_authn", names: ["service_assertion_replay"] },
      to: ["zeroship_workflow"],
    });
    workflowPlatformPolicy();
    for (const [id, fingerprint] of [["workflow", fingerprints.postgres], ["platform_policy", fingerprints.platform_policy]]) {
      raw({
        sql: `INSERT INTO workflow.schema_version (id,fingerprint) VALUES ('${id}','${fingerprint}')`,
        reason: "runtime verifies the migration-compiler fingerprint without DDL privileges",
      });
    }
  },
};
