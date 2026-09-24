import { grant, now, raw, revoke, t, table } from "@zeroship/migrate";

// The workflow service's own policy storage: the operator switches it reads and
// the publication ledger it writes. Both sit in `workflow_manager` because the
// service is the only reader and the only writer, so the ledger write is a
// local write rather than one that would have to cross a service boundary.
//
// This file sorts after 20260911000000_workflow_coordination.ts, which creates
// the schema and the two roles, and it is separate from the generated manager
// schema for a reason: `workflowManagerSchema` grants its tables the full
// SELECT/INSERT/UPDATE/DELETE set that `Coordinator::verify` probes for, and
// these two deliberately carry less. The service never deletes a ledger row and
// never writes the rollout config at all.
//
// The rollout config has no service writer: an operator publishes the `global`
// row with an administrative credential (docs/runbooks/workflows.md). It is not
// reachable from `zeroship_control`, whose access to this whole schema is
// revoked, and after the database split control could not reach this store at
// all.
const schema = "workflow_manager";
const policyTables = ["workflow_rollout_config", "workflow_policy_ledger"];

function wm(name) {
  return table(name, { schema });
}

export default {
  name: "workflow_policy_tables",
  schema() {
    wm("workflow_rollout_config").create({
      columns: {
        id: t.text().required().default("global"),
        dispatch_paused: t.boolean().required().default(false),
        ingress_disabled: t.boolean().required().default(false),
        source_validity_ms: t.bigInt().required(),
        updated_at: t.timestamp().required().default(now()),
        updated_by: t.text(),
      },
      primaryKey: ["id"],
    });
    wm("workflow_rollout_config").check("workflow_rollout_config_id_check").add({ expr: (c) => c("id").eq("global") });
    wm("workflow_rollout_config").check("workflow_rollout_config_validity_check").add({ expr: (c) => c("source_validity_ms").gt(0) });

    wm("workflow_policy_ledger").create({
      columns: {
        id: t.text().required(),
        revision: t.bigInt().required().default(0),
        policy_json: t.json(),
        source_validity_ms: t.bigInt(),
      },
      primaryKey: ["id"],
    });
    wm("workflow_policy_ledger").check("workflow_policy_ledger_publication_check").add({
      expr: (c) => c("revision").eq(0).and(c("policy_json").isNull(), c("source_validity_ms").isNull())
        .or(c("revision").gt(0).and(c("policy_json").isNotNull(), c("source_validity_ms").isNotNull(), c("source_validity_ms").gt(0))),
    });
    // The ledger is keyed by app id, a sortable typed id, so it needs the
    // bytewise comparison the platform's identity domains use. The rollout
    // config's id is the constant "global" and is not an identity domain.
    raw({
      sql: `ALTER TABLE "${schema}"."workflow_policy_ledger" ALTER COLUMN "id" TYPE text COLLATE "C"`,
      reason: "typed-id text domains need bytewise comparison",
    });

    for (const name of policyTables) {
      raw({
        sql: `ALTER TABLE ${schema}."${name}" OWNER TO zeroship_workflow_migrator`,
        reason: "workflow metadata ownership belongs to the migration role",
      });
    }
    grant({ privileges: ["select"], on: { kind: "table", schema, names: ["workflow_rollout_config"] }, to: ["zeroship_workflow"] });
    grant({ privileges: ["select", "insert", "update"], on: { kind: "table", schema, names: ["workflow_policy_ledger"] }, to: ["zeroship_workflow"] });
    revoke({
      privileges: ["all"],
      on: { kind: "table", schema, names: policyTables },
      from: ["PUBLIC", "zeroship_control", "zeroship_worker", "zeroship_gateway", "zeroship_app"],
    });
  },
};
