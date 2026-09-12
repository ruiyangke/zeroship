import { raw, table, t } from "../../../packages/zero-migrate/dist/index.js";

// Identity domains and every stored copy use the bytewise convention from
// db/migrations-ts/20260831000001_sortable_entity_id_collations.ts. This map
// belongs beside creation because the earlier platform migration cannot see it.
export const coordinatorIdentityColumns = {
  workers: ["worker_id"],
  scopes: ["app_id"],
  assignments: ["app_id", "worker_id"],
  placement_receipts: ["app_id", "request_id", "worker_id"],
  management: ["app_id", "request_id", "run_id", "ack_worker_id"],
};

// This schema holds coordination metadata only. Customer journals and payload
// locations are deliberately absent, including from command/receipt storage.
export function workflowCoordinatorSchema() {
  const namespace = "workflow_coordination";
  const tables = [];
  const text = () => t.text().notNull();
  const integer = () => t.bigInt().notNull();
  const fk = (name, columns, target, targetColumns) => ({
    name, columns, references: { table: target, schema: namespace, columns: targetColumns }, onDelete: "restrict",
  });
  const create = (name, columns, primaryKey, foreignKeys = []) => {
    tables.push(name);
    table(name, { schema: namespace }).create({ columns, primaryKey, foreignKeys });
  };
  const index = (name, purpose, on) => table(name, { schema: namespace }).index(`${name}_${purpose}_idx`).add({ on });
  create("schema_version", { id: text(), fingerprint: text() }, ["id"]);
  create("workers", {
    worker_id: text(), capacity: integer(), state: text(), expires_at: integer(),
  }, ["worker_id"]);
  create("scopes", { app_id: text() }, ["app_id"]);
  create("assignments", {
    app_id: text(), worker_id: text(), revision: integer(), expires_at: integer(),
    released: t.boolean().notNull(), wake_revision: t.bigInt(), next_due_at: t.bigInt(),
  }, ["app_id", "worker_id"], [
    fk("assignment_scope", ["app_id"], "scopes", ["app_id"]),
    fk("assignment_worker", ["worker_id"], "workers", ["worker_id"]),
  ]);
  index("assignments", "worker", ["worker_id", "app_id"]);
  index("assignments", "expiry", ["expires_at", "app_id"]);
  create("placement_receipts", {
    app_id: text(), request_id: text(), operation: text(), worker_id: text(),
    expected_revision: t.bigInt(), wake_revision: t.bigInt(),
    result_revision: integer(), result_expires_at: integer(),
  }, ["app_id", "request_id"], [fk("receipt_scope", ["app_id"], "scopes", ["app_id"])]);
  create("management", {
    app_id: text(), request_id: text(), run_id: text(), actor: text(), operation: text(),
    restart_name: t.text(), restart_occurrence: t.bigInt(), restart_deploy: t.text(),
    created_at: integer(), outcome: t.text(), run_state: t.text(),
    ack_worker_id: t.text(), ack_revision: t.bigInt(),
  }, ["app_id", "request_id"], [fk("management_scope", ["app_id"], "scopes", ["app_id"])]);
  index("management", "pending", ["app_id", "outcome", "created_at", "request_id"]);

  for (const [name, columns] of Object.entries(coordinatorIdentityColumns)) {
    raw({
      sql: `ALTER TABLE ${namespace}."${name}" ${columns.map(column => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`).join(", ")}`,
      reason: "coordinator identities and their copies require bytewise comparison",
    });
  }
  return tables;
}
