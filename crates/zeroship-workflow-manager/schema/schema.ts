import { dialect, raw, table, t } from "../../../packages/zero-migrate/dist/index.js";

// Keep identity columns and their foreign-key copies bytewise, following the
// platform's sortable_entity_id_collations migration convention.
export const managerIdentityColumns = {
  schema_version: ["id"],
  queue_scopes: ["id"],
  workers: ["id"],
  assignments: ["id", "app_id", "worker_id"],
  placement_receipts: ["id", "app_id", "request_id", "worker_id"],
  management: ["id", "app_id", "request_id", "run_id", "ack_worker_id"],
  jobs: ["id", "app_id", "deployment_id", "worker_id"],
  recovery_scopes: ["id", "deployment_id", "pending_job_id"],
};

// Queue records carry closed job metadata; customer history and payloads stay
// in the worker's creator database and object storage.
export function workflowManagerSchema(namespace) {
  const text = () => t.text().notNull();
  const integer = () => t.bigInt().notNull();
  const fk = (name, columns, target, targetColumns) => ({
    name, columns, references: { table: target, schema: namespace, columns: targetColumns }, onDelete: "restrict",
  });
  const create = (name, columns, domainKey, foreignKeys = []) => {
    const collection = table(name, { schema: namespace });
    collection.create({
      columns: { id: text(), ...columns }, primaryKey: ["id"], foreignKeys,
    });
    if (domainKey.length !== 1 || domainKey[0] !== "id") {
      collection.index(`${name}_scope_key`).add({ on: domainKey, unique: true });
    }
  };
  const index = (name, purpose, on) => table(name, { schema: namespace }).index(`${name}_${purpose}_idx`).add({ on });
  create("schema_version", { fingerprint: text() }, ["id"]);
  const scopes = table("queue_scopes", { schema: namespace });
  scopes.create({
    columns: {
      id: text(),
      lock_version: integer().default(0),
    },
    primaryKey: ["id"],
  });
  create("workers", {
    capacity: integer(), state: text(), expires_at: integer(),
    lock_version: integer().default(0),
  }, ["id"]);
  create("assignments", {
    app_id: text(), worker_id: text(), revision: integer(), expires_at: integer(),
    released: t.boolean().notNull(), wake_revision: t.bigInt(), next_due_at: t.bigInt(),
  }, ["app_id", "worker_id"], [
    fk("assignment_scope", ["app_id"], "queue_scopes", ["id"]),
    fk("assignment_worker", ["worker_id"], "workers", ["id"]),
  ]);
  index("assignments", "worker", ["worker_id", "app_id"]);
  index("assignments", "expiry", ["expires_at", "app_id"]);
  create("placement_receipts", {
    app_id: text(), request_id: text(), operation: text(), worker_id: text(),
    expected_revision: t.bigInt(), wake_revision: t.bigInt(),
    result_revision: integer(), result_expires_at: integer(),
  }, ["app_id", "request_id"], [fk("receipt_scope", ["app_id"], "queue_scopes", ["id"])]);
  create("management", {
    app_id: text(), request_id: text(), run_id: text(), actor: text(), operation: text(),
    restart_name: t.text(), restart_occurrence: t.bigInt(), restart_deploy: t.text(),
    created_at: integer(), outcome: t.text(), run_state: t.text(),
    ack_worker_id: t.text(), ack_revision: t.bigInt(),
  }, ["app_id", "request_id"], [fk("management_scope", ["app_id"], "queue_scopes", ["id"])]);
  index("management", "pending", ["app_id", "outcome", "created_at", "request_id"]);

  const jobs = table("jobs", { schema: namespace });
  jobs.create({
    columns: {
      id: text(),
      app_id: text(),
      deployment_id: text(),
      operation: text(),
      spec_digest: text(),
      available_at: integer(),
      state: text(),
      attempt: integer().default(0),
      worker_id: t.text(),
      assignment_revision: t.bigInt(),
      lease_deadline: t.bigInt(),
      outcome: t.text(),
      settlement_digest: t.text(),
      created_at: integer(),
    },
    primaryKey: ["id"],
    foreignKeys: [{
      name: "jobs_app_id_fkey",
      columns: ["app_id"],
      references: { schema: namespace, table: "queue_scopes", columns: ["id"] },
      onDelete: "restrict",
    }],
  });
  jobs.index("jobs_available_idx").add({ on: ["app_id", "state", "available_at", "id"] });
  jobs.index("jobs_lease_idx").add({ on: ["app_id", "state", "lease_deadline", "id"] });

  create("recovery_scopes", {
    deployment_id: text(), activation_revision: integer(), next_due_at: integer(),
    pending_job_id: t.text(),
  }, ["id"], [
    fk("recovery_scope", ["id"], "queue_scopes", ["id"]),
    fk("recovery_job", ["pending_job_id"], "jobs", ["id"]),
  ]);
  index("recovery_scopes", "due", ["next_due_at", "id"]);

  // ColumnDef does not yet expose the engine's portable bytewise collation
  // facet. Keep this PostgreSQL-specific DDL in the migration recorder, where
  // the compiler authorizes and emits it. SQLite text defaults to BINARY.
  dialect({
    postgres() {
      for (const [name, columns] of Object.entries(managerIdentityColumns)) {
        raw({
          sql: `ALTER TABLE "${namespace}"."${name}" ${columns.map(column => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`).join(", ")}`,
          reason: "workflow metadata identities and their copies require bytewise comparison",
        });
      }
    },
    sqlite() {},
  });
  return Object.keys(managerIdentityColumns);
}
