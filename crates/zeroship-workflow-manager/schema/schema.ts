import { dialect, raw, table, t } from "../../../packages/zero-migrate/dist/index.js";

// Keep identity columns and their foreign-key copies bytewise, following the
// platform's sortable_entity_id_collations migration convention.
export const managerIdentityColumns = {
  schema_version: ["id"],
  queue_scopes: ["id"],
  deployment_holds: ["id", "app_id", "deployment_id", "holder_id"],
  workers: ["id"],
  assignments: ["id", "app_id", "worker_id"],
  placement_receipts: ["id", "app_id", "request_id", "worker_id"],
  management: ["id", "app_id", "request_id", "run_id"],
  management_scopes: ["id", "app_id", "run_id"],
  jobs: ["id", "app_id", "deployment_id", "worker_id", "run_id", "management_request_id"],
  recovery_scopes: ["id", "deployment_id"],
  recovery_duties: ["id", "app_id", "pending_job_id"],
  schedule_deployments: ["id", "app_id"],
  schedule_activations: ["id", "app_id", "deployment_id"],
  schedule_disables: ["id", "app_id"],
  schedule_scopes: ["id", "activation_id"],
  schedules: ["id", "app_id", "name", "activation_id"],
  schedule_occurrences: ["id", "app_id", "schedule_id", "run_id", "job_id", "activation_id"],
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
      dispatch_cursor: integer().default(0),
    },
    primaryKey: ["id"],
  });
  // held_at is the manager time of the latest transition to held. The release
  // policy leaves a hold alone until it is older than the queue transaction
  // budget, so an acquirer that confirmed it outside its transaction commits first.
  create("deployment_holds", {
    app_id: text(), deployment_id: text(), holder_id: text(),
    deploy_hash: t.text(), generation: integer(), state: text(), held_at: t.bigInt(),
  }, ["app_id", "deployment_id"], [
    fk("deployment_hold_scope", ["app_id"], "queue_scopes", ["id"]),
  ]);
  index("deployment_holds", "pending", ["state", "app_id", "deployment_id"]);
  create("workers", {
    // Identity-only upserts lock existing registrations without resetting their
    // state. A new row remains ineligible until registration sets its liveness.
    capacity: integer().default(1), state: text().default("ready"), expires_at: integer().default(0),
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
  const jobs = table("jobs", { schema: namespace });
  jobs.create({
    columns: {
      id: text(),
      app_id: text(),
      deployment_id: t.text(),
      operation: text(),
      operation_kind: text(),
      management_request_id: t.text(),
      run_id: t.text(),
      spec_digest: text(),
      available_at: integer(),
      dispatch_order: integer(),
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
  jobs.index("jobs_scope_key").add({ on: ["app_id", "id"], unique: true });
  jobs.index("jobs_deployment_idx").add({ on: ["app_id", "deployment_id", "state"] });
  jobs.index("jobs_dispatch_key").add({ on: ["app_id", "dispatch_order"], unique: true });
  jobs.index("jobs_dispatch_idx").add({ on: ["app_id", "state", "dispatch_order"] });

  jobs.index("jobs_management_request_key").add({ on: ["app_id", "management_request_id"], unique: true });
  jobs.index("jobs_operation_idx").add({ on: ["app_id", "operation_kind", "state", "id"] });

  create("management_scopes", {
    app_id: text(), run_id: text(), accepted_revision: integer(), settled_revision: integer(),
  }, ["app_id", "run_id"], [fk("management_order_app", ["app_id"], "queue_scopes", ["id"])]);
  create("management", {
    app_id: text(), request_id: text(), run_id: text(), revision: integer(), actor: text(),
    request: text(), request_digest: text(), blocks_execution: t.boolean().notNull(),
    created_at: integer(), outcome: t.text(),
  }, ["app_id", "request_id"], [
    fk("management_job", ["app_id", "id"], "jobs", ["app_id", "id"]),
    fk("management_order", ["app_id", "run_id"], "management_scopes", ["app_id", "run_id"]),
  ]);
  table("management", { schema: namespace }).index("management_revision_key").add({ on: ["app_id", "run_id", "revision"], unique: true });
  index("management", "pending", ["app_id", "outcome", "id"]);
  index("management", "barrier", ["app_id", "run_id", "outcome", "blocks_execution", "revision"]);

  create("schedule_deployments", {
    app_id: text(), definition: text(), interpretation: text(), created_at: integer(),
  }, ["app_id", "id"], [fk("schedule_deployment_scope", ["app_id"], "queue_scopes", ["id"])]);
  create("schedule_activations", {
    app_id: text(), deployment_id: text(), revision: integer(), activated_at: integer(),
  }, ["app_id", "revision"], [
    fk("schedule_activation_job", ["app_id", "id"], "jobs", ["app_id", "id"]),
    fk("schedule_activation_deploy", ["app_id", "deployment_id"], "schedule_deployments", ["app_id", "id"]),
  ]);
  table("schedule_activations", { schema: namespace }).index("schedule_activations_identity_key").add({ on: ["app_id", "id"], unique: true });
  index("schedule_activations", "deployment", ["app_id", "deployment_id", "id"]);
  create("schedule_disables", {
    app_id: text(), revision: integer(), created_at: integer(),
  }, ["app_id", "revision"], [fk("schedule_disable_scope", ["app_id"], "queue_scopes", ["id"])]);
  create("schedule_scopes", {
    revision: integer(), enabled: t.boolean().notNull(), activation_id: t.text(),
  }, ["id"], [
    fk("schedule_scope_app", ["id"], "queue_scopes", ["id"]),
    fk("schedule_scope_activation", ["id", "activation_id"], "schedule_activations", ["app_id", "id"]),
  ]);
  create("schedules", {
    app_id: text(), name: text(), activation_id: text(), revision: integer(),
    definition: text(), next_at: t.bigInt(), anchor_at: integer(),
    catch_up_until: t.bigInt(), catch_up_remaining: t.bigInt(),
  }, ["app_id", "name"], [
    fk("schedule_activation", ["app_id", "activation_id"], "schedule_activations", ["app_id", "id"]),
  ]);
  table("schedules", { schema: namespace }).index("schedules_identity_key").add({ on: ["app_id", "id"], unique: true });
  index("schedules", "due", ["next_at", "id"]);
  index("schedules", "activation", ["app_id", "activation_id"]);
  create("schedule_occurrences", {
    app_id: text(), schedule_id: text(), revision: integer(), scheduled_at: integer(),
    run_id: text(), job_id: text(), activation_id: text(),
  }, ["app_id", "schedule_id", "revision", "scheduled_at"], [
    fk("occurrence_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
    fk("occurrence_job", ["app_id", "job_id"], "jobs", ["app_id", "id"]),
    fk("occurrence_activation", ["app_id", "activation_id"], "schedule_activations", ["app_id", "id"]),
  ]);
  table("schedule_occurrences", { schema: namespace }).index("schedule_occurrences_job_key").add({ on: ["app_id", "job_id"], unique: true });

  create("recovery_scopes", {
    deployment_id: text(), activation_revision: integer(),
  }, ["id"], [
    fk("recovery_scope", ["id"], "queue_scopes", ["id"]),
  ]);
  create("recovery_duties", {
    app_id: text(), kind: text(), next_due_at: integer(), pending_job_id: t.text(),
  }, ["app_id", "kind"], [
    fk("recovery_duty_scope", ["app_id"], "recovery_scopes", ["id"]),
    fk("recovery_duty_job", ["app_id", "pending_job_id"], "jobs", ["app_id", "id"]),
  ]);
  index("recovery_duties", "due", ["kind", "next_due_at", "app_id"]);

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
