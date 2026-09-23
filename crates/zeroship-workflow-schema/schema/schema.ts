import { dialect, table, t } from "../../../packages/zero-migrate/dist/index.js";

// Cursor order and identity copies compare these columns as bytes, so their
// order must not move with the database's default collation. The facet carries
// the intent; the compiler spells it per dialect (PostgreSQL `COLLATE "C"`,
// SQLite `COLLATE BINARY`).
//
// The set is deliberately narrow: a column earns a place here by being compared
// in a cursor or range scan, not by being text, and it is NOT closed over the
// foreign keys that reference it.
const bytewiseColumns = {
  app_state: [
    "collection_after_id", "collection_upper_id",
    "reconciliation_after_id", "reconciliation_upper_id",
  ],
  job_publications: ["id", "app_id"],
  advance_publications: ["id", "app_id", "run_id", "deploy_id"],
  fanout_publications: ["id", "app_id", "broadcast_id"],
  propagation_publications: ["id", "app_id"],
  fanout_pages: ["id", "app_id", "broadcast_id"],
  propagation_pages: ["id", "app_id"],
  broadcasts: ["id", "app_id", "topic"],
  topics: ["app_id", "topic"],
  deployment_holds: ["deploy_id"],
  job_receipts: ["id", "app_id", "run_id"],
  collection_pages: ["id", "app_id"],
  reconciliation_pages: ["id", "app_id"],
  payloads: ["id", "app_id"],
  activations: ["id", "app_id", "deploy_id"],
  activation_scopes: ["id", "activation_id"],
  management_receipts: ["id", "app_id", "run_id", "request_id"],
  schedules: ["id", "app_id", "name"],
  occurrences: ["id", "app_id", "schedule_id", "job_id", "run_id"],
  tasks: ["job_id"],
};

// The customer owns the journal. Provisioning supplies its resolved schema;
// both database adapters use the canonical definition and reserved table names.
export function workflowSchema(namespace) {
  const identifiers = new Set();
  const columnsInSchema = new Set();
  const bytewisePending = new Set(Object.keys(bytewiseColumns));
  const owned = name => { identifiers.add(name); return name; };
  const text = () => t.text().required();
  const integer = () => t.bigInt().required();
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
    bytewisePending.delete(name);
    for (const column of bytewiseColumns[name] ?? []) {
      if (!(column in columns)) throw new Error(`${name}.${column} is pinned bytewise but is not a column of ${name}`);
      columns[column] = columns[column].collation("bytewise");
    }
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

  // The journal's stamp: ONE row, whatever the schema holds. `version` names the
  // point in the ordered series the installed journal has reached, so an
  // installer can tell an out-of-date journal from a corrupted one, and refuse
  // to write an older series over a newer journal.
  create("schema_version", { id: text(), version: integer(), fingerprint: text() }, ["id"]);
  // closed_epoch is the highest manager ingress epoch a delivered Close fenced.
  // Ingress acceptance requires its captured epoch to exceed it under this
  // row's lock; it never moves backwards.
  //
  // The collection_* and reconciliation_* columns are this app's two paged
  // sweeps. Each is a per-app singleton read and compare-and-set under the same
  // lock the sweep already takes, so it is a column of the locked row rather
  // than a satellite table keyed by the app id. Their defaults are the state a
  // sweep starts from, so registering the app is the only write that creates
  // one.
  create("app_state", {
    ...identity(), signal_epoch: integer().default(0),
    last_polled_at: integer().default(0),
    subscription_sequence: integer().default(0),
    signal_sequence: integer().default(0),
    closed_epoch: integer().default(0),
    collection_revision: integer().default(1),
    collection_after_id: t.text(), collection_upper_id: t.text(),
    collection_observed_at: t.bigInt(),
    reconciliation_revision: integer().default(1),
    reconciliation_phase: text().default("publications"),
    reconciliation_after_id: t.text(), reconciliation_upper_id: t.text(),
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
  // The manager delivers work per app, so polling range-scans one app's due
  // frontier in run identity order. One schema holds many apps; app identity
  // leads every journal index.
  index("runs", "due", ["app_id", "due_at", "id"]);
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
  // A column lives here only when code that has not yet determined the job's
  // kind reads it: `run_id` answers the kind-blind left join that finds runs
  // with no outstanding receipt. Everything a single kind stores lives in that
  // kind's own table, keyed by the job id and scoped by the app.
  create("job_receipts", {
    ...identity(), run_id: t.text(), id: text(), specification: text(), outcome: t.text(),
    created_at: integer(), completed_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("job_receipts")]);
  index("job_receipts", "run", ["app_id", "run_id"]);
  // Both paged sweeps store the same extension: the plan the delivered page
  // committed to, and the index of the next item it will reserve.
  create("collection_pages", {
    ...identity(), plan: text(), next_index: integer(),
  }, ["app_id", "id"], [
    appFk("collection_pages"),
    fk("collection_page_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ]);
  create("reconciliation_pages", {
    ...identity(), plan: text(), next_index: integer(),
  }, ["app_id", "id"], [
    appFk("reconciliation_pages"),
    fk("reconciliation_page_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ]);
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
  // The revision identity is also the ordering the head lookup reads: one run's
  // applied management revisions are unique per app, so the highest of them is
  // an index-ordered first row rather than a scan.
  create("management_receipts", {
    ...runIdentity(), request_id: text(), revision: integer(), outcome: text(), created_at: integer(),
  }, ["app_id", "request_id"], [
    appFk("management_receipts"),
    fk("management_job_receipt", ["app_id", "id"], "job_receipts", ["app_id", "id"]),
  ], [{ name: "management_revision_identity", columns: ["app_id", "run_id", "revision"] }]);
  create("occurrences", {
    ...identity(), schedule_id: text(), revision: integer(), at: integer(), job_id: text(), run_id: t.text(),
  }, ["app_id", "schedule_id", "revision", "at"], [
    fk("occurrence_schedule", ["app_id", "schedule_id"], "schedules", ["app_id", "id"]),
    fk("occurrence_job", ["app_id", "job_id"], "job_receipts", ["app_id", "id"]),
    fk("occurrence_run", ["app_id", "run_id"], "runs", ["app_id", "id"]),
  ], [{ name: "occurrence_job_identity", columns: ["app_id", "job_id"] }]);
  // run_id, generation and task_id are STAGING-LOCATION metadata, not durable
  // ownership; `payload_refs` on (app_id, run_id, generation, slot, ordinal) is
  // that. Bytes have to be stageable BEFORE any run exists -- a continuation
  // seed, a child run's input, a root run a request handler or the cron sweep
  // starts -- so all three are nullable and `app_id` alone scopes the tenant.
  // Both composite keys stay: MATCH SIMPLE, which is what both dialects apply,
  // satisfies a key carrying a NULL without a lookup, so an owned row is still
  // checked against its generation and task and an ownerless one skips it.
  create("payloads", {
    ...identity(), run_id: t.text(), generation: t.bigInt(),
    id: text(), task_id: t.text(), request_id: text(), hash: text(), size: integer(),
    content_type: t.text(), state: text(), created_at: integer(), expires_at: integer(),
  }, ["app_id", "id"], [
    generationFk("payloads"),
    fk("payload_task", ["app_id", "run_id", "generation", "task_id"], "tasks", ["app_id", "run_id", "generation", "id"]),
  ]);
  table("payloads", { schema: namespace }).index(owned("payload_upload_request")).add({ on: ["app_id", "task_id", "request_id"], unique: true });
  create("payload_refs", {
    ...generation(), slot: text(), ordinal: integer(), payload_id: text(),
  }, ["app_id", "run_id", "generation", "slot", "ordinal"], [
    generationFk("payload_refs"),
    fk("payload_ref", ["app_id", "payload_id"], "payloads", ["app_id", "id"]),
  ]);
  create("outbox", {
    ...identity(), id: text(), kind: text(), payload: text(), created_at: integer(), delivered_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("outbox")]);
  // Only Advance, Fanout and Propagate are ever published, and this row holds
  // what the sweep that has not yet read the specification needs: the intent's
  // identity, the specification itself, and whether a manager receipt confirmed
  // it. Each operation's own projection is a row in its own table, so a kind
  // reaches its own columns and no other's, and the key that deduplicates that
  // kind is total there rather than a unique index over a nullable group.
  create("job_publications", {
    ...identity(), specification: text(),
    created_at: integer(), confirmed_at: t.bigInt(),
  }, ["app_id", "id"], [appFk("job_publications")]);
  index("job_publications", "pending", ["app_id", "confirmed_at", "id"]);
  // An advance intent's due time is part of its identity: a run whose frontier
  // revision has not moved but whose due time has is a different job, so the
  // deduplication key carries it.
  create("advance_publications", {
    ...identity(), deploy_id: text(), run_id: text(), generation: integer(),
    frontier_revision: integer(), available_at: integer(),
  }, ["app_id", "id"], [
    appFk("advance_publications"),
    fk("advance_publication_intent", ["app_id", "id"], "job_publications", ["app_id", "id"]),
  ], [
    { name: "advance_publication_frontier", columns: ["app_id", "run_id", "generation", "frontier_revision", "available_at"] },
  ]);
  index("advance_publications", "deployment", ["app_id", "deploy_id"]);
  create("fanout_publications", {
    ...identity(), broadcast_id: text(), revision: integer(),
  }, ["app_id", "id"], [
    appFk("fanout_publications"),
    fk("fanout_publication_intent", ["app_id", "id"], "job_publications", ["app_id", "id"]),
  ], [
    { name: "fanout_publication_broadcast", columns: ["app_id", "broadcast_id", "revision"] },
  ]);
  create("propagation_publications", {
    ...identity(), propagation_id: text(), revision: integer(),
  }, ["app_id", "id"], [
    appFk("propagation_publications"),
    fk("propagation_publication_intent", ["app_id", "id"], "job_publications", ["app_id", "id"]),
  ], [
    { name: "propagation_publication_obligation", columns: ["app_id", "propagation_id", "revision"] },
  ]);
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
  // A map entry naming a table this schema does not create would pin nothing and
  // say so nowhere, so the unconsumed keys are an error rather than a no-op.
  if (bytewisePending.size) {
    throw new Error(`bytewise pins name tables the journal does not create: ${[...bytewisePending].join(", ")}`);
  }
  return { identifiers, columns: columnsInSchema };
}
