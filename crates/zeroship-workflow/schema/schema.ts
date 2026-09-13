import { dialect, raw, table, t } from "../../../packages/zero-migrate/dist/index.js";

// The customer owns the journal. Provisioning supplies its resolved schema;
// both database adapters use the canonical definition and reserved table names.
export function workflowSchema(namespace) {
  const identifiers = new Set();
  const columnsInSchema = new Set();
  const owned = name => { identifiers.add(name); return name; };
  const text = () => t.text().notNull();
  const integer = () => t.bigInt().notNull();
  const identity = () => ({ app_id: text() });
  const runIdentity = () => ({ ...identity(), run_id: text() });
  const generation = () => ({ ...runIdentity(), generation: integer() });
  const fk = (name, columns, target, targetColumns, onDelete = "restrict") => ({
    name: owned(name), columns, references: { table: target, schema: namespace, columns: targetColumns }, onDelete,
  });
  const appFk = (name) => fk(`${name}_app`, ["app_id"], "app_state", ["app_id"]);
  const runFk = (name) => fk(`${name}_run`, ["app_id", "run_id"], "runs", ["app_id", "id"]);
  const generationFk = (name) => fk(`${name}_generation`, ["app_id", "run_id", "generation"], "generations", ["app_id", "run_id", "generation"]);
  const create = (name, columns, domainKey, foreignKeys = [], uniques = []) => {
    owned(name);
    columns = { id: text(), ...columns };
    Object.keys(columns).forEach(column => columnsInSchema.add(column));
    const domainUnique = domainKey.length === 1 && domainKey[0] === "id"
      ? []
      : [{ name: owned(`${name}_scope_key`), on: domainKey, unique: true }];
    const definition = { columns, primaryKey: ["id"], foreignKeys, indexes: domainUnique };
    const selfReferences = foreignKeys.filter(key => key.references.table === name);
    if (selfReferences.length) {
      // PostgreSQL needs the scoped unique index before adding self references;
      // SQLite needs those references declared inside CREATE TABLE.
      dialect({
        postgres: () => {
          table(name, { schema: namespace }).create({
            ...definition, foreignKeys: foreignKeys.filter(key => key.references.table !== name),
          });
          for (const { name: constraint, ...reference } of selfReferences) {
            table(name, { schema: namespace }).foreignKey(constraint).add(reference);
          }
        },
        sqlite: () => { table(name, { schema: namespace }).create(definition); },
      });
    } else {
      table(name, { schema: namespace }).create(definition);
    }
    for (const unique of uniques) {
      table(name, { schema: namespace }).index(owned(unique.name)).add({ on: unique.columns, unique: true });
    }
  };
  const index = (name, purpose, columns) => table(name, { schema: namespace }).index(owned(`${name}_${purpose}_idx`)).add({ on: columns });

  create("schema_version", { id: text(), fingerprint: text() }, ["id"]);
  create("app_state", {
    ...identity(), signal_epoch: integer().default(0),
    last_polled_at: integer().default(0),
    subscription_sequence: integer().default(0),
  }, ["app_id"]);
  create("deploys", {
    ...identity(), id: text(), hash: text(), manifest: text(), created_at: integer(),
    active: integer(), state: text(),
    availability_epoch: integer(),
  }, ["app_id", "id"], [appFk("deploys")], [
    { name: "deploy_hash_identity", columns: ["app_id", "hash"] },
  ]);
  create("deployment_holds", {
    ...identity(), deploy_id: text(), deploy_hash: text(), holder_id: text(),
    generation: integer(), state: text(),
  }, ["app_id", "deploy_id"], [appFk("deployment_holds")]);
  index("deployment_holds", "pending", ["app_id", "state", "deploy_id"]);
  create("schedules", {
    ...identity(), id: text(), name: text(), workflow_name: text(), deploy_id: text(), definition: text(),
    next_at: t.bigInt(), revision: integer(), anchor_at: integer(), last_checked_at: integer(),
  }, ["app_id", "id"], [
    appFk("schedules"),
    fk("schedule_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
  ], [{ name: "schedule_name", columns: ["app_id", "name"] }]);
  index("schedules", "due", ["next_at", "app_id", "id"]);
  create("runs", {
    ...identity(), id: text(), workflow_name: text(), deploy_id: text(),
    generation: integer(), state: text(), control: text(), due_at: t.bigInt(),
    task_id: t.text(), lease_epoch: integer(), frontier_revision: integer().default(1), key: t.text(),
    parent_id: t.text(), parent_generation: t.bigInt(), parent_ordinal: t.bigInt(),
    cascade: integer(), depth: integer(), created_at: integer(), terminal_at: t.bigInt(),
    signal_epoch: integer(), compensation_target: t.text(), schedule_id: t.text(),
    continued_from_id: t.text(), continued_to_id: t.text(),
  }, ["app_id", "id"], [
    appFk("runs"),
    fk("run_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
    fk("run_parent", ["app_id", "parent_id"], "runs", ["app_id", "id"]),
    fk("run_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
    fk("run_continued_from", ["app_id", "continued_from_id"], "runs", ["app_id", "id"]),
    fk("run_continued_to", ["app_id", "continued_to_id"], "runs", ["app_id", "id"]),
  ], [{ name: "live_workflow_key", columns: ["app_id", "workflow_name", "key"] }]);
  index("runs", "due", ["due_at", "app_id", "id"]);
  index("runs", "parent", ["app_id", "parent_id", "parent_generation"]);
  create("generations", {
    ...generation(), deploy_id: text(), input: text(), input_ref: t.text(), output: t.text(), output_ref: t.text(), error: t.text(),
    state: text(), started_at: integer(), terminal_at: t.bigInt(),
  }, ["app_id", "run_id", "generation"], [
    runFk("generations"),
    fk("generation_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
  ]);
  create("steps", {
    ...generation(), ordinal: integer(), name: text(), occurrence: integer(),
    origin_generation: integer(), kind: text(), state: text(), record: text(),
    compensation_attempts: integer().default(0), compensation_due_at: t.bigInt(),
    compensation_error: t.text(), compensation_retry_ms: integer().default(1000),
  }, ["app_id", "run_id", "generation", "ordinal"], [generationFk("steps")], [
    { name: "step_name_occurrence", columns: ["app_id", "run_id", "generation", "name", "occurrence"] },
  ]);
  create("tasks", {
    ...generation(), id: text(), worker: text(), epoch: integer(), token_hash: text(),
    deadline: integer(), state: text(), completion_digest: t.text(), receipt: t.text(),
    created_at: integer(), finished_at: t.bigInt(),
  }, ["id"], [generationFk("tasks")], [
    { name: "task_scope_identity", columns: ["app_id", "run_id", "generation", "id"] },
    { name: "task_fence_identity", columns: ["app_id", "run_id", "generation", "epoch"] },
  ]);
  index("tasks", "admission", ["app_id", "state", "deadline"]);
  create("waits", {
    ...generation(), ordinal: integer(), kind: text(), signal_type: t.text(),
    topic: t.text(), max_signal_age: t.bigInt(), due_at: t.bigInt(), child_id: t.text(),
  }, ["app_id", "run_id", "generation", "ordinal"], [
    fk("wait_step", ["app_id", "run_id", "generation", "ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
    fk("wait_child", ["app_id", "child_id"], "runs", ["app_id", "id"]),
  ]);
  create("topics", {
    ...identity(), topic: text(), signal_epoch: integer(),
  }, ["app_id", "topic"], [appFk("topics")]);
  create("broadcasts", {
    ...identity(), id: text(), topic: text(), signal_type: text(), payload: text(),
    created_at: integer(), cursor: integer(), cutoff_sequence: integer(),
    origin: text(), finished: integer(),
  }, ["app_id", "id"], [appFk("broadcasts")]);
  create("signals", {
    ...runIdentity(), id: text(), signal_type: text(), payload: text(),
    created_at: integer(), consumed_generation: t.bigInt(), consumed_ordinal: t.bigInt(),
    broadcast_id: t.text(),
    origin: text().default("app"), delivery: text().default("direct"), topic: t.text(),
    target_generation: t.bigInt(), target_ordinal: t.bigInt(),
  }, ["app_id", "id"], [
    runFk("signals"),
    fk("signal_consumption", ["app_id", "run_id", "consumed_generation", "consumed_ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
    fk("signal_broadcast", ["app_id", "broadcast_id"], "broadcasts", ["app_id", "id"]),
    fk("signal_target", ["app_id", "run_id", "target_generation", "target_ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
  ], [{ name: "broadcast_delivery", columns: ["app_id", "broadcast_id", "run_id"] }]);
  index("signals", "mailbox", ["app_id", "run_id", "signal_type", "consumed_generation", "created_at"]);
  create("subscriptions", {
    ...generation(), ordinal: integer(), id: text(), topic: text(), created_at: integer(), sequence: integer(),
  }, ["app_id", "run_id", "generation", "ordinal"], [
    fk("subscription_step", ["app_id", "run_id", "generation", "ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
  ], [
    { name: "subscription_identity", columns: ["app_id", "id"] },
    { name: "subscription_sequence_unique", columns: ["app_id", "sequence"] },
  ]);
  index("subscriptions", "topic", ["app_id", "topic", "id"]);
  create("requests", {
    ...identity(), request_id: text(), operation: text(), digest: text(), result: text(), created_at: integer(),
  }, ["app_id", "request_id"], [appFk("requests")]);
  create("management_receipts", {
    ...identity(), request_id: text(), digest: text(), outcome: text(), created_at: integer(),
  }, ["app_id", "request_id"], [appFk("management_receipts")]);
  create("occurrences", {
    ...identity(), schedule_id: text(), at: integer(), run_id: t.text(),
  }, ["app_id", "schedule_id", "at"], [
    fk("occurrence_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
    fk("occurrence_run", ["app_id", "run_id"], "runs", ["app_id", "id"]),
  ]);
  create("payloads", {
    ...generation(), id: text(), task_id: text(), request_id: text(), hash: text(), size: integer(),
    content_type: t.text(), state: text(), created_at: integer(), expires_at: integer(),
  }, ["app_id", "id"], [
    generationFk("payloads"),
    fk("payload_task", ["app_id", "run_id", "generation", "task_id"], "tasks", ["app_id", "run_id", "generation", "id"]),
  ]);
  table("payloads", { schema: namespace }).index(owned("payload_upload_request")).add({ on: ["app_id", "task_id", "request_id"], unique: true });
  index("payloads", "expiry", ["state", "expires_at"]);
  create("payload_refs", {
    ...generation(), slot: text(), ordinal: integer(), payload_id: text(),
  }, ["app_id", "run_id", "generation", "slot", "ordinal"], [
    generationFk("payload_refs"),
    fk("payload_ref", ["app_id", "payload_id"], "payloads", ["app_id", "id"]),
  ]);
  create("outbox", {
    ...identity(), id: text(), kind: text(), payload: text(), created_at: integer(), delivered_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("outbox")]);
  index("outbox", "delivery", ["delivered_at", "created_at"]);
  create("job_publications", {
    ...runIdentity(), id: text(), deploy_id: text(), generation: integer(),
    frontier_revision: integer(), available_at: integer(), specification: text(),
    created_at: integer(), confirmed_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("job_publications")], [
    { name: "job_publication_frontier", columns: ["app_id", "run_id", "generation", "frontier_revision", "available_at"] },
  ]);
  index("job_publications", "pending", ["app_id", "confirmed_at", "id"]);
  index("job_publications", "deployment", ["app_id", "deploy_id", "confirmed_at"]);
  // The migration DSL does not expose the column collation facet yet. Cursor
  // order and identity copies must use bytewise comparison; SQLite uses BINARY.
  dialect({
    postgres() {
      raw({
        sql: `ALTER TABLE "${namespace}"."job_publications" ${["id", "app_id", "run_id", "deploy_id"].map(column => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`).join(", ")}`,
        reason: "workflow publication identities require bytewise comparison",
      });
    },
    sqlite() {},
  });
  return { identifiers, columns: columnsInSchema };
}
