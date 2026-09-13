import { grant, raw, revoke, role, schema } from "@zeroship/migrate";
import { workflowManagerSchema } from "../../crates/zeroship-workflow-manager/schema/schema.ts";
import { readFileSync } from "node:fs";

export default {
  name: "workflow_coordination",
  schema() {
    role("zeroship_workflow_migrator").create({ login: false });
    role("zeroship_workflow").create({ login: true, setSearchPath: ["workflow_manager", "pg_catalog"] });
    schema("workflow_manager").create({ authorization: "zeroship_workflow_migrator" });
    const managerTables = workflowManagerSchema("workflow_manager");
    if (!managerTables.length) throw new Error("workflow manager schema is empty");
    for (const name of managerTables) {
      raw({
        sql: `ALTER TABLE workflow_manager."${name.replaceAll('"', '""')}" OWNER TO zeroship_workflow_migrator`,
        reason: "workflow metadata ownership belongs to the migration role",
      });
    }
    grant({ privileges: ["usage"], on: { kind: "schema", names: ["workflow_manager"] }, to: ["zeroship_workflow"] });
    grant({ privileges: ["select"], on: { kind: "table", schema: "workflow_manager", names: ["schema_version"] }, to: ["zeroship_workflow"] });
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: { kind: "table", schema: "workflow_manager", names: managerTables.filter(name => name !== "schema_version") },
      to: ["zeroship_workflow"],
    });
    revoke({ privileges: ["all"], on: { kind: "schema", names: ["workflow_manager"] }, from: ["PUBLIC", "zeroship_control", "zeroship_worker", "zeroship_gateway", "zeroship_app"] });
    revoke({
      privileges: ["all"],
      on: { kind: "table", schema: "workflow_manager", names: managerTables },
      from: ["PUBLIC", "zeroship_control", "zeroship_worker", "zeroship_gateway", "zeroship_app"],
    });
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
    const fingerprint = readFileSync(new URL("../../crates/zeroship-workflow-manager/schema/fingerprint.txt", import.meta.url), "utf8").trim();
    if (!/^[a-f0-9]{64}$/.test(fingerprint)) throw new Error("invalid manager schema fingerprint");
    raw({
      sql: `INSERT INTO workflow_manager.schema_version (id,fingerprint) VALUES ('manager','${fingerprint}')`,
      reason: "runtime verifies canonical metadata schema without DDL privileges",
    });
  },
};
