import { and, membership, notMembership, or, table, t } from "@zeroship/migrate";
import { grant, revoke } from "@zeroship/migrate/pg";

export const name = "durable_workflows_journal";

const schema = "zeroship";
const journalTables = [
  "app_deploys",
  "workflow_runs",
  "workflow_blobs",
  "workflow_signal_keys",
  "workflow_broadcasts",
  "workflow_steps",
  "workflow_signals",
  "workflow_subscriptions",
  "workflow_schedules",
];

function zs(name) {
  return table(name, { schema });
}

function journalTableTarget() {
  return { kind: "table", schema, names: journalTables };
}

export function up() {
  zs("app_deploys").create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      deploy_hash: t.text().notNull(),
      manifest_json: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      activated_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  zs("app_deploys").unique("app_deploys_app_id_deploy_hash_key").add({ columns: ["app_id", "deploy_hash"] });
  zs("app_deploys").index("app_deploys_app_created_idx").add({
    columns: ["app_id", { kind: "column", name: "created_at", order: "desc" }],
  });

  zs("workflow_runs").create({
    columns: {
      id: t.text().notNull(),
      workflow_name: t.text().notNull(),
      app_id: t.uuid().notNull(),
      deploy_id: t.text().notNull(),
      state: t.text().notNull(),
      input: t.json(),
      output: t.json(),
      error: t.json(),
      output_kind: t.text().notNull().default("inline"),
      output_hash: t.char(64),
      output_size: t.bigInt(),
      output_content_type: t.text(),
      input_hash: t.char(64),
      input_size: t.bigInt(),
      input_content_type: t.text(),
      journal_bytes: t.bigInt().notNull().default(0),
      blob_bytes: t.bigInt().notNull().default(0),
      wake_at: t.timestamp(),
      claimed_by: t.text(),
      claim_epoch: t.integer().notNull().default(0),
      lease_expires: t.timestamp(),
      last_dispatch_at: t.timestamp(),
      concurrency: t.smallInt().notNull().default(1),
      next_ordinal: t.integer().notNull().default(0),
      stuck_strikes: t.smallInt().notNull().default(0),
      signal_epoch: t.integer().notNull().default(0),
      parent_run_id: t.text(),
      parent_wait_step_key: t.text(),
      parent_cascade: t.boolean().notNull().default(false),
      tree_depth: t.smallInt().notNull().default(0),
      cancel_requested: t.boolean().notNull().default(false),
      compensation_target: t.text(),
      compensation_outcome: t.text(),
      restart_count: t.smallInt().notNull().default(0),
      restarted_at: t.timestamp(),
      restarted_from_ordinal: t.integer(),
      restarted_by: t.text(),
      dedup_key: t.text(),
      started_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  zs("workflow_runs").addCheck("workflow_runs_state_check", (c) =>
    membership(c("state"), ["queued", "running", "sleeping", "waiting", "paused", "stalled", "compensating", "completed", "failed", "cancelled"]),
  );
  zs("workflow_runs").addCheck("workflow_runs_compensation_target_check", (c) => membership(c("compensation_target"), ["failed", "cancelled"]));
  zs("workflow_runs").addCheck("workflow_runs_compensation_outcome_check", (c) => membership(c("compensation_outcome"), ["completed", "partial"]));
  zs("workflow_runs").addCheck("workflow_runs_check", (c) =>
    or(
      and(c("output_kind").eq("inline"), c("output_hash").isNull()),
      and(c("output_kind").eq("blob"), c("output_hash").isNotNull(), c("output_size").isNotNull(), c("output").isNull(), c("output_hash").matches("^[0-9a-f]{64}$")),
    ),
  );
  zs("workflow_runs").addCheck("workflow_runs_check1", (c) => or(c("input").isNotNull(), c("input_hash").isNotNull()));
  zs("workflow_runs").addCheck("workflow_runs_check2", (c) => c("parent_run_id").isNull().eq(c("parent_wait_step_key").isNull()));
  zs("workflow_runs").unique("workflow_runs_app_id_workflow_name_dedup_key_key").add({ columns: ["app_id", "workflow_name", "dedup_key"] });
  zs("workflow_runs").index("workflow_runs_wake_due_idx").add({
    columns: ["wake_at"],
    where: (c) => and(membership(c("state"), ["sleeping", "waiting", "compensating"]), c("wake_at").isNotNull()),
  });
  zs("workflow_runs").index("workflow_runs_lease_sweep_idx").add({
    columns: ["lease_expires"],
    where: (c) => c("claimed_by").isNotNull(),
  });
  zs("workflow_runs").index("workflow_runs_live_children_idx").add({
    columns: ["parent_run_id"],
    where: (c) => and(c("parent_run_id").isNotNull(), notMembership(c("state"), ["completed", "failed", "cancelled", "stalled"])),
  });
  zs("workflow_runs").index("workflow_runs_cancel_requested_idx").add({
    columns: ["id"],
    where: (c) => c("cancel_requested"),
  });

  zs("workflow_blobs").create({
    columns: {
      hash: t.char(64).notNull(),
      size: t.bigInt().notNull(),
      content_type: t.text().notNull(),
      refcount: t.integer().notNull().default(0),
      first_seen_at: t.timestamp().notNull().default({ fn: "now" }),
      last_referenced_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["hash"],
  });
  zs("workflow_blobs").addCheck("workflow_blobs_hash_check", (c) => c("hash").matches("^[0-9a-f]{64}$"));
  zs("workflow_blobs").index("workflow_blobs_gc_idx").add({
    columns: ["last_referenced_at"],
    where: (c) => c("refcount").eq(0),
  });

  zs("workflow_signal_keys").create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      kid: t.text().notNull(),
      verifier: t.text().notNull(),
      secret_ct: t.bytes().notNull(),
      secret_kek: t.text().notNull(),
      status: t.text().notNull().default("active"),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      rotated_at: t.timestamp(),
      retired_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  zs("workflow_signal_keys").addCheck("workflow_signal_keys_verifier_check", (c) => membership(c("verifier"), ["zeroship-hmac", "bearer-signing", "provider:stripe"]));
  zs("workflow_signal_keys").addCheck("workflow_signal_keys_status_check", (c) => membership(c("status"), ["active", "next", "retiring", "retired"]));
  zs("workflow_signal_keys").unique("workflow_signal_keys_app_id_kid_key").add({ columns: ["app_id", "kid"] });
  zs("workflow_signal_keys").index("workflow_signal_keys_app_status_idx").add({ columns: ["app_id", "status"] });

  zs("workflow_broadcasts").create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      topic: t.text().notNull(),
      type: t.text().notNull(),
      payload: t.json().notNull(),
      origin: t.text().notNull(),
      provider: t.text(),
      idempotency_key: t.text().notNull(),
      deploy_id: t.text().notNull(),
      fanout_state: t.text().notNull().default("pending"),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
    },
    primaryKey: ["id"],
  });
  zs("workflow_broadcasts").addCheck("workflow_broadcasts_origin_check", (c) => membership(c("origin"), ["app", "ingress"]));
  zs("workflow_broadcasts").addCheck("workflow_broadcasts_fanout_state_check", (c) => membership(c("fanout_state"), ["pending", "completed"]));
  zs("workflow_broadcasts").unique("workflow_broadcasts_app_id_topic_idempotency_key_key").add({ columns: ["app_id", "topic", "idempotency_key"] });
  zs("workflow_broadcasts").index("workflow_broadcasts_pending_fanout_idx").add({
    columns: ["app_id", "topic", "created_at"],
    where: (c) => c("fanout_state").eq("pending"),
  });
  zs("workflow_broadcasts").index("workflow_broadcasts_expires_idx").add({ columns: ["expires_at"] });

  zs("workflow_steps").create({
    columns: {
      run_id: t.text().notNull(),
      ordinal: t.integer().notNull(),
      name: t.text().notNull(),
      name_occurrence: t.integer().notNull().default(0),
      kind: t.text().notNull(),
      state: t.text().notNull(),
      attempt: t.integer().notNull().default(0),
      max_attempts: t.integer().notNull().default(1),
      output: t.json(),
      error: t.json(),
      output_kind: t.text().notNull().default("inline"),
      output_hash: t.char(64),
      output_size: t.bigInt(),
      output_content_type: t.text(),
      wake_at: t.timestamp(),
      signal_type: t.text(),
      max_signal_age: t.text(),
      consumed_signal_id: t.text(),
      child_run_id: t.text(),
      batch_id: t.text().notNull(),
      batch_width: t.smallInt().notNull().default(1),
      started_at: t.timestamp().notNull().default({ fn: "now" }),
      finished_at: t.timestamp(),
      compensation_state: t.text(),
      compensation_attempt: t.integer().notNull().default(0),
      compensation_max_attempts: t.integer().notNull().default(1),
      compensation_wake_at: t.timestamp(),
      compensation_error: t.json(),
      compensation_batch_id: t.text(),
      compensation_finished_at: t.timestamp(),
    },
    primaryKey: ["run_id", "ordinal"],
  });
  zs("workflow_steps").addCheck("workflow_steps_kind_check", (c) => membership(c("kind"), ["run", "sleep", "wait_signal", "child"]));
  zs("workflow_steps").addCheck("workflow_steps_state_check", (c) => membership(c("state"), ["running", "completed", "failed"]));
  zs("workflow_steps").addCheck("workflow_steps_check", (c) =>
    or(
      and(c("output_kind").eq("inline"), c("output_hash").isNull()),
      and(c("output_kind").eq("blob"), c("output_hash").isNotNull(), c("output_size").isNotNull(), c("output").isNull(), c("output_hash").matches("^[0-9a-f]{64}$")),
    ),
  );
  zs("workflow_steps").addCheck("workflow_steps_compensation_state_check", (c) =>
    or(c("compensation_state").isNull(), membership(c("compensation_state"), ["pending", "running", "completed", "failed"])),
  );
  zs("workflow_steps").addCheck("workflow_steps_check1", (c) => or(c("compensation_state").isNull(), c("kind").eq("run")));
  zs("workflow_steps").unique("workflow_steps_run_id_name_name_occurrence_key").add({ columns: ["run_id", "name", "name_occurrence"] });
  zs("workflow_steps").index("workflow_steps_running_wake_idx").add({
    columns: ["run_id", "wake_at"],
    where: (c) => and(c("state").eq("running"), c("wake_at").isNotNull()),
  });
  zs("workflow_steps").index("workflow_steps_compensation_frontier_idx").add({
    columns: ["run_id", { kind: "column", name: "ordinal", order: "desc" }],
    where: (c) => membership(c("compensation_state"), ["pending", "running"]),
  });
  zs("workflow_steps").index("workflow_steps_compensation_wake_idx").add({
    columns: ["run_id", "compensation_wake_at"],
    where: (c) => and(c("compensation_state").eq("running"), c("compensation_wake_at").isNotNull()),
  });

  zs("workflow_signals").create({
    columns: {
      id: t.text().notNull(),
      run_id: t.text().notNull(),
      type: t.text().notNull(),
      payload: t.json(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      consumed_by: t.text(),
      origin: t.text().notNull().default("app"),
      delivery: t.text().notNull().default("direct"),
      topic: t.text(),
      broadcast_id: t.text(),
      idempotency_key: t.text(),
      provider: t.text(),
    },
    primaryKey: ["id"],
  });
  zs("workflow_signals").addCheck("workflow_signals_origin_check", (c) => membership(c("origin"), ["app", "ingress", "system"]));
  zs("workflow_signals").addCheck("workflow_signals_delivery_check", (c) => membership(c("delivery"), ["direct", "topic"]));
  zs("workflow_signals").addCheck("workflow_signals_check", (c) => c("delivery").eq("topic").eq(c("topic").isNotNull()));
  zs("workflow_signals").index("workflow_signals_pending_idx").add({
    columns: ["run_id", "type"],
    where: (c) => c("consumed_by").isNull(),
  });
  zs("workflow_signals").index("workflow_signals_ext_idem_uidx").add({
    columns: ["run_id", "type", "idempotency_key"],
    unique: true,
    where: (c) => and(c("idempotency_key").isNotNull(), c("delivery").ne("topic")),
  });
  zs("workflow_signals").index("workflow_signals_bcast_run_uidx").add({
    columns: ["broadcast_id", "run_id"],
    unique: true,
    where: (c) => c("broadcast_id").isNotNull(),
  });

  zs("workflow_subscriptions").create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      topic: t.text().notNull(),
      run_id: t.text().notNull(),
      signal_name: t.text().notNull(),
      type_filter: t.text(),
      ordinal: t.integer().notNull(),
      max_age_ms: t.bigInt(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  zs("workflow_subscriptions").unique("workflow_subscriptions_run_id_ordinal_key").add({ columns: ["run_id", "ordinal"] });
  zs("workflow_subscriptions").index("workflow_subscriptions_app_topic_idx").add({ columns: ["app_id", "topic"] });

  zs("workflow_schedules").create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      deploy_id: t.text().notNull(),
      name: t.text().notNull(),
      workflow_name: t.text().notNull(),
      kind: t.text().notNull(),
      cron_expr: t.text(),
      tz: t.text(),
      interval_ms: t.bigInt(),
      anchor: t.text(),
      input_json: t.json().notNull().default({}),
      overlap: t.text().notNull().default("allow"),
      catchup: t.text().notNull().default("skip"),
      catchup_max: t.integer().notNull().default(0),
      next_fire_at: t.timestamp().notNull(),
      last_fire_at: t.timestamp(),
      paused: t.boolean().notNull().default(false),
      claimed_by: t.text(),
      claimed_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  zs("workflow_schedules").addCheck("workflow_schedules_kind_check", (c) => membership(c("kind"), ["cron", "interval"]));
  zs("workflow_schedules").addCheck("workflow_schedules_anchor_check", (c) => membership(c("anchor"), ["epoch", "deploy"]));
  zs("workflow_schedules").addCheck("workflow_schedules_overlap_check", (c) => membership(c("overlap"), ["allow", "skipIfRunning"]));
  zs("workflow_schedules").addCheck("workflow_schedules_catchup_check", (c) => membership(c("catchup"), ["skip", "backfill"]));
  zs("workflow_schedules").addCheck("workflow_schedules_catchup_max_check", (c) => c("catchup_max").ge(0));
  zs("workflow_schedules").addCheck("workflow_schedules_check", (c) =>
    or(
      and(c("kind").eq("cron"), c("cron_expr").isNotNull(), c("tz").isNotNull(), c("interval_ms").isNull(), c("anchor").isNull()),
      and(c("kind").eq("interval"), c("interval_ms").isNotNull(), c("anchor").isNotNull(), c("cron_expr").isNull(), c("tz").isNull()),
    ),
  );
  zs("workflow_schedules").unique("workflow_schedules_app_id_name_key").add({ columns: ["app_id", "name"] });
  zs("workflow_schedules").index("workflow_schedules_due_idx").add({
    columns: ["next_fire_at"],
    where: (c) => c("paused").not(),
  });
  zs("workflow_schedules").index("workflow_schedules_app_idx").add({ columns: ["app_id"] });

  zs("app_deploys").foreignKey("app_deploys_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_runs").foreignKey("workflow_runs_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_runs").foreignKey("workflow_runs_deploy_id_fkey").add({ columns: ["deploy_id"], references: { table: "app_deploys", columns: ["id"] } });
  zs("workflow_runs").foreignKey("workflow_runs_parent_run_id_fkey").add({ columns: ["parent_run_id"], references: { table: "workflow_runs", columns: ["id"] }, onDelete: "restrict" });
  zs("workflow_signal_keys").foreignKey("workflow_signal_keys_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_broadcasts").foreignKey("workflow_broadcasts_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_broadcasts").foreignKey("workflow_broadcasts_deploy_id_fkey").add({ columns: ["deploy_id"], references: { table: "app_deploys", columns: ["id"] } });
  zs("workflow_steps").foreignKey("workflow_steps_run_id_fkey").add({ columns: ["run_id"], references: { table: "workflow_runs", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_signals").foreignKey("workflow_signals_run_id_fkey").add({ columns: ["run_id"], references: { table: "workflow_runs", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_signals").foreignKey("workflow_signals_broadcast_id_fkey").add({ columns: ["broadcast_id"], references: { table: "workflow_broadcasts", columns: ["id"] } });
  zs("workflow_subscriptions").foreignKey("workflow_subscriptions_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_subscriptions").foreignKey("workflow_subscriptions_run_id_fkey").add({ columns: ["run_id"], references: { table: "workflow_runs", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_schedules").foreignKey("workflow_schedules_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
  zs("workflow_schedules").foreignKey("workflow_schedules_deploy_id_fkey").add({ columns: ["deploy_id"], references: { table: "app_deploys", columns: ["id"] }, onDelete: "cascade" });

  revoke({ privileges: ["all"], on: journalTableTarget(), from: ["public"] });
  grant({ privileges: ["select", "insert", "update", "delete"], on: journalTableTarget(), to: ["zeroship_control"] });
  revoke({ privileges: ["insert", "update", "delete"], on: journalTableTarget(), from: ["zeroship_gateway", "zeroship_worker", "zeroship_app"] });
}

export function down() {
  revoke({ privileges: ["all"], on: journalTableTarget(), from: ["zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_app"] });

  zs("workflow_schedules").drop({ ifExists: true });
  zs("workflow_subscriptions").drop({ ifExists: true });
  zs("workflow_signals").drop({ ifExists: true });
  zs("workflow_steps").drop({ ifExists: true });
  zs("workflow_broadcasts").drop({ ifExists: true });
  zs("workflow_signal_keys").drop({ ifExists: true });
  zs("workflow_blobs").drop({ ifExists: true });
  zs("workflow_runs").drop({ ifExists: true });
  zs("app_deploys").drop({ ifExists: true });
}
