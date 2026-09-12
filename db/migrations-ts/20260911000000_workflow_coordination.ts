import { grant, raw, revoke, role, schema } from "@zeroship/migrate";
import { workflowCoordinatorSchema } from "../../crates/zeroship-workflow-server/schema/schema.ts";
import { readFileSync } from "node:fs";

export default {
  name: "workflow_coordination",
  schema() {
    role("zeroship_workflow_migrator").create({ login: false });
    role("zeroship_workflow").create({ login: true, setSearchPath: ["workflow_coordination", "pg_catalog"] });
    schema("workflow_coordination").create({ authorization: "zeroship_workflow_migrator" });
    const tables = workflowCoordinatorSchema();
    for (const name of tables) {
      raw({
        sql: `ALTER TABLE workflow_coordination."${name}" OWNER TO zeroship_workflow_migrator`,
        reason: "coordination metadata ownership belongs to the migration role",
      });
    }
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["workflow_coordination"] }, to: ["zeroship_workflow"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "workflow_coordination", names: ["schema_version"] }, to: ["zeroship_workflow"] });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "workflow_coordination", names: tables.filter(name => name !== "schema_version") },
      to: ["zeroship_workflow"],
    });
    revoke({ privileges: ["all"], on: { kind: "schema", names: ["workflow_coordination"] }, from: ["PUBLIC", "zeroship_control", "zeroship_worker", "zeroship_gateway", "zeroship_app"] });
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship", "service_authn"] }, to: ["zeroship_workflow"] });
    raw({
      sql: "GRANT SELECT (id,status,public_key) ON zeroship.worker_instances TO zeroship_workflow",
      reason: "the coordinator verifies enrolled worker identity without customer-data access",
    });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "service_authn", names: ["service_assertion_replay"] },
      to: ["zeroship_workflow"],
    });
    const fingerprint = readFileSync(new URL("../../crates/zeroship-workflow-server/schema/fingerprint.txt", import.meta.url), "utf8").trim();
    if (!/^[a-f0-9]{64}$/.test(fingerprint)) throw new Error("invalid coordinator schema fingerprint");
    raw({
      sql: `INSERT INTO workflow_coordination.schema_version (id,fingerprint) VALUES ('coordination','${fingerprint}')`,
      reason: "runtime verifies canonical metadata schema without DDL privileges",
    });
  },
};
