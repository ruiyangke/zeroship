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
    // Declared uniques sit beside the scope key, so a unique index covering a
    // foreign key's columns replaces that key's supporting index.
    const indexes = [
      ...domainUnique,
      ...uniques.map(unique => ({ name: owned(unique.name), on: unique.columns, unique: true })),
    ];
    const definition = { columns, primaryKey: ["id"], foreignKeys, indexes };
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
  };
  const index = (name, purpose, columns) => table(name, { schema: namespace }).index(owned(`${name}_${purpose}_idx`)).add({ on: columns });

  create("schema_version", { id: text(), fingerprint: text() }, ["id"]);
  create("app_state", {
    ...identity(), signal_epoch: integer().default(0),
    last_polled_at: integer().default(0),
    subscription_sequence: integer().default(0),
    signal_sequence: integer().default(0),
  }, ["app_id"]);
  create("deploys", {
    ...identity(), id: text(), hash: text(), manifest: text(), created_at: integer(),
    active: integer(), state: text(),
    availability_epoch: integer(),
  }, ["app_id", "id"], [appFk("deploys")], [
    { name: "deploy_hash_identity", columns: ["app_id", "hash"] },
  ]);
  create("deployment_holds", {
    ...identity(), deploy_id: text(), deploy_hash: t.text(), holder_id: text(),
    generation: integer(), state: text(),
  }, ["app_id", "deploy_id"], [appFk("deployment_holds")]);
  index("deployment_holds", "pending", ["app_id", "state", "deploy_id"]);
  create("schedules", {
    ...identity(), id: text(), name: text(),
  }, ["app_id", "id"], [appFk("schedules")], [
    { name: "schedule_name", columns: ["app_id", "name"] },
  ]);
  create("runs", {
    ...identity(), id: text(), workflow_name: text(), deploy_id: text(),
    generation: integer(), state: text(), control: text(), due_at: t.bigInt(),
    task_id: t.text(), lease_epoch: integer(), frontier_revision: integer().default(1), key: t.text(),
    parent_id: t.text(), parent_generation: t.bigInt(), parent_ordinal: t.bigInt(),
    cascade: integer(), depth: integer(), created_at: integer(), terminal_at: t.bigInt(),
    signal_epoch: integer(), compensation_target: t.text(), schedule_id: t.text(),
  }, ["app_id", "id"], [
    appFk("runs"),
    fk("run_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
    fk("run_parent", ["app_id", "parent_id"], "runs", ["app_id", "id"]),
    fk("run_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
  ], [{ name: "live_workflow_key", columns: ["app_id", "workflow_name", "key"] }]);
  index("runs", "due", ["due_at", "app_id", "id"]);
  // Cascade propagation pages range-scan one generation's cascading children
  // in run identity order.
  index("runs", "parent", ["app_id", "parent_id", "parent_generation", "cascade", "id"]);
  create("generations", {
    ...generation(), deploy_id: text(), input: text(), input_ref: t.text(), output: t.text(), output_ref: t.text(), error: t.text(),
    state: text(), started_at: integer(), terminal_at: t.bigInt(),
  }, ["app_id", "run_id", "generation"], [
    runFk("generations"),
    fk("generation_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
  ], [{ name: "generation_identity", columns: ["app_id", "id"] }]);
  create("continuation_heads", {
    ...identity(), current_generation_id: text(), revision: integer(),
  }, ["app_id", "id"], [
    fk("continuation_head_generation", ["app_id", "current_generation_id"], "generations", ["app_id", "id"]),
  ]);
  create("continuation_members", {
    ...identity(), head_id: text(), revision: integer(),
  }, ["app_id", "id"], [
    fk("continuation_member_generation", ["app_id", "id"], "generations", ["app_id", "id"]),
    fk("continuation_member_head", ["app_id", "head_id"], "continuation_heads", ["app_id", "id"]),
  ], [{ name: "continuation_member_revision", columns: ["app_id", "head_id", "revision"] }]);
  create("steps", {
    ...generation(), ordinal: integer(), name: text(), occurrence: integer(),
    origin_generation: integer(), kind: text(), state: text(), record: text(),
    child_member_id: t.text(), child_result_member_id: t.text(),
    compensation_attempts: integer().default(0), compensation_due_at: t.bigInt(),
    compensation_error: t.text(), compensation_retry_ms: integer().default(1000),
  }, ["app_id", "run_id", "generation", "ordinal"], [
    generationFk("steps"),
    fk("step_child_member", ["app_id", "child_member_id"], "continuation_members", ["app_id", "id"]),
    fk("step_child_result_member", ["app_id", "child_result_member_id"], "continuation_members", ["app_id", "id"]),
  ], [
    { name: "step_name_occurrence", columns: ["app_id", "run_id", "generation", "name", "occurrence"] },
  ]);
  // Child checkpoints always name their accepted member, so a completing head
  // reaches every waiting parent through that reference. The compiler renders
  // table checks for PostgreSQL only, so SQLite journals rely on the writer.
  dialect({
    postgres() {
      table("steps", { schema: namespace }).check("step_child_linkage").add({
        expr: col => col("kind").eq("child").and(col("child_member_id").isNotNull())
          .or(col("kind").ne("child").and(col("child_member_id").isNull(), col("child_result_member_id").isNull())),
      });
    },
    sqlite() {},
  });
  create("job_receipts", {
    ...identity(), run_id: t.text(), id: text(), specification: text(), outcome: t.text(),
    reconciliation: t.text(), reconciliation_next: t.bigInt(),
    created_at: integer(), completed_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("job_receipts")]);
  index("job_receipts", "run", ["app_id", "run_id"]);
  create("collection_pages", {
    ...identity(), plan: text(), next_index: integer(),
  }, ["app_id", "id"], [
    appFk("collection_pages"),
    fk("collection_page_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ]);
  create("collection_scans", {
    revision: integer(), after_id: t.text(), upper_id: t.text(), observed_at: t.bigInt(),
  }, ["id"], [fk("collection_scan_app", ["id"], "app_state", ["app_id"])]);
  create("activations", {
    ...identity(), deploy_id: text(), revision: integer(),
  }, ["app_id", "revision"], [
    fk("activation_deploy", ["app_id", "deploy_id"], "deploys", ["app_id", "id"]),
    fk("activation_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ], [{ name: "activation_identity", columns: ["app_id", "id"] }]);
  create("activation_scopes", {
    activation_id: text(), revision: integer(),
  }, ["id"], [
    fk("activation_scope_app", ["id"], "app_state", ["app_id"]),
    fk("activation_scope_receipt", ["id", "activation_id"], "activations", ["app_id", "id"]),
  ]);
  create("tasks", {
    ...generation(), id: text(), worker: text(), epoch: integer(), token_hash: text(),
    deadline: integer(), state: text(), completion_digest: t.text(), receipt: t.text(),
    frontier_revision: integer().default(1), job_id: t.text(),
    delivery_attempt: t.bigInt(), assignment_revision: t.bigInt(),
    created_at: integer(), finished_at: t.bigInt(),
  }, ["id"], [generationFk("tasks"),
    fk("task_job", ["app_id", "job_id"], "job_receipts", ["app_id", "id"]),
  ], [
    { name: "task_scope_identity", columns: ["app_id", "run_id", "generation", "id"] },
    { name: "task_fence_identity", columns: ["app_id", "run_id", "generation", "epoch"] },
  ]);
  index("tasks", "admission", ["app_id", "state", "deadline"]);
  create("waits", {
    ...generation(), ordinal: integer(), kind: text(), signal_type: t.text(),
    topic: t.text(), max_signal_age: t.bigInt(), due_at: t.bigInt(),
  }, ["app_id", "run_id", "generation", "ordinal"], [
    fk("wait_step", ["app_id", "run_id", "generation", "ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
  ]);
  create("topics", {
    ...identity(), topic: text(), signal_epoch: integer(),
    accepted_sequence: integer().default(0), completed_sequence: integer().default(0),
  }, ["app_id", "topic"], [appFk("topics")]);
  create("broadcasts", {
    ...identity(), id: text(), topic: text(), signal_type: text(), payload: text(),
    created_at: integer(), cursor: integer(), cutoff_sequence: integer(),
    origin: text(), finished: integer(),
    sequence: integer(), revision: integer(),
  }, ["app_id", "id"], [appFk("broadcasts"),
    fk("broadcast_topic", ["app_id", "topic"], "topics", ["app_id", "topic"]),
  ], [{ name: "broadcast_order", columns: ["app_id", "topic", "sequence"] }]);
  create("signals", {
    ...runIdentity(), id: text(), signal_type: text(), payload: text(),
    created_at: integer(), delivery_sequence: integer(), consumed_generation: t.bigInt(), consumed_ordinal: t.bigInt(),
    broadcast_id: t.text(),
    origin: text().default("app"), delivery: text().default("direct"), topic: t.text(),
    target_generation: t.bigInt(), target_ordinal: t.bigInt(),
  }, ["app_id", "id"], [
    runFk("signals"),
    fk("signal_consumption", ["app_id", "run_id", "consumed_generation", "consumed_ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
    fk("signal_broadcast", ["app_id", "broadcast_id"], "broadcasts", ["app_id", "id"]),
    fk("signal_target", ["app_id", "run_id", "target_generation", "target_ordinal"], "steps", ["app_id", "run_id", "generation", "ordinal"]),
  ], [{ name: "broadcast_delivery", columns: ["app_id", "broadcast_id", "run_id"] },
    { name: "signal_delivery_order", columns: ["app_id", "delivery_sequence"] },
  ]);
  index("signals", "mailbox", ["app_id", "run_id", "signal_type", "consumed_generation", "delivery_sequence"]);
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
    ...runIdentity(), request_id: text(), revision: integer(), digest: text(), outcome: text(), created_at: integer(),
  }, ["app_id", "request_id"], [
    appFk("management_receipts"),
    fk("management_job_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ], [{ name: "management_revision_identity", columns: ["app_id", "run_id", "revision"] }]);
  create("management_scopes", {
    ...runIdentity(), revision: integer(),
  }, ["app_id", "run_id"], [
    appFk("management_scopes"),
    fk("management_scope_head", ["app_id", "run_id", "revision"], "management_receipts", ["app_id", "run_id", "revision"]),
  ]);
  create("occurrences", {
    ...identity(), schedule_id: text(), revision: integer(), at: integer(), job_id: text(), run_id: t.text(),
  }, ["app_id", "schedule_id", "revision", "at"], [
    fk("occurrence_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
    fk("occurrence_job", ["app_id", "job_id"], "job_receipts", ["app_id", "id"]),
    fk("occurrence_run", ["app_id", "run_id"], "runs", ["app_id", "id"]),
  ], [{ name: "occurrence_job_identity", columns: ["app_id", "job_id"] }]);
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
    ...identity(), id: text(), run_id: t.text(), deploy_id: t.text(), generation: t.bigInt(),
    frontier_revision: t.bigInt(), broadcast_id: t.text(), broadcast_revision: t.bigInt(),
    propagation_id: t.text(), propagation_revision: t.bigInt(),
    available_at: integer(), specification: text(),
    created_at: integer(), confirmed_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("job_publications")], [
    { name: "job_publication_frontier", columns: ["app_id", "run_id", "generation", "frontier_revision", "available_at"] },
    { name: "job_publication_fanout", columns: ["app_id", "broadcast_id", "broadcast_revision"] },
    { name: "job_publication_propagation", columns: ["app_id", "propagation_id", "propagation_revision"] },
  ]);
  index("job_publications", "pending", ["app_id", "confirmed_at", "id"]);
  index("job_publications", "deployment", ["app_id", "deploy_id", "confirmed_at"]);
  create("fanout_pages", {
    ...identity(), broadcast_id: text(), revision: integer(), result: text(),
  }, ["app_id", "id"], [
    fk("fanout_page_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
    fk("fanout_page_publication", ["app_id", "id"], "job_publications", ["app_id", "id"]),
    fk("fanout_page_broadcast", ["app_id", "broadcast_id"], "broadcasts", ["app_id", "id"]),
  ], [{ name: "fanout_page_revision", columns: ["app_id", "broadcast_id", "revision"] }]);
  // One obligation per source generation and kind. Its scope key also answers
  // the cancellation fence lookup for a cascading child's parent generation.
  create("propagations", {
    ...generation(), kind: text(), cursor: t.text(), revision: integer(),
    finished: integer(), created_at: integer(),
  }, ["app_id", "run_id", "generation", "kind"], [generationFk("propagations")], [
    { name: "propagation_identity", columns: ["app_id", "id"] },
  ]);
  create("propagation_pages", {
    ...identity(), propagation_id: text(), revision: integer(), result: text(),
  }, ["app_id", "id"], [
    fk("propagation_page_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
    fk("propagation_page_publication", ["app_id", "id"], "job_publications", ["app_id", "id"]),
    fk("propagation_page_obligation", ["app_id", "propagation_id"], "propagations", ["app_id", "id"]),
  ], [{ name: "propagation_page_revision", columns: ["app_id", "propagation_id", "revision"] }]);
  create("reconciliation_scans", {
    revision: integer(), phase: text(), after_id: t.text(), upper_id: t.text(),
  }, ["id"], [fk("reconciliation_scan_app", ["id"], "app_state", ["app_id"])]);
  // The migration DSL does not expose the column collation facet yet. Cursor
  // order and identity copies must use bytewise comparison; SQLite uses BINARY.
  dialect({
    postgres() {
      for (const [name, columns] of Object.entries({
        job_publications: ["id", "app_id", "run_id", "deploy_id", "broadcast_id"],
        fanout_pages: ["id", "app_id", "broadcast_id"],
        propagation_pages: ["id", "app_id"],
        broadcasts: ["id", "app_id", "topic"],
        topics: ["app_id", "topic"],
        deployment_holds: ["deploy_id"],
        job_receipts: ["id", "app_id", "run_id"],
        collection_pages: ["id", "app_id"],
        collection_scans: ["id", "after_id", "upper_id"],
        payloads: ["id", "app_id"],
        activations: ["id", "app_id", "deploy_id"],
        activation_scopes: ["id", "activation_id"],
        management_receipts: ["id", "app_id", "run_id", "request_id"],
        management_scopes: ["id", "app_id", "run_id"],
        schedules: ["id", "app_id", "name"],
        occurrences: ["id", "app_id", "schedule_id", "job_id", "run_id"],
        reconciliation_scans: ["id", "after_id", "upper_id"],
        tasks: ["job_id"],
      })) {
        raw({
          sql: `ALTER TABLE "${namespace}"."${name}" ${columns.map(column => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`).join(", ")}`,
          reason: "workflow delivery identities require bytewise comparison",
        });
      }
    },
    sqlite() {},
  });
  return { identifiers, columns: columnsInSchema };
}
