import { table, t, now, grant } from "@zeroship/migrate";
import { deploymentSchema } from "../../crates/zeroship-workflow-manager/schema/deployments/schema.ts";

const schema = "zeroship";
const journalTables = [
  "app_deploys",
  "app_deploy_holds",
  "workflow_runs",
  "workflow_blobs",
  "workflow_signal_keys",
  "workflow_broadcasts",
  "workflow_steps",
  "workflow_signals",
  "workflow_subscriptions",
  "workflow_schedules",
  "workflow_rollout_config",
  "workflow_policy_ledger",
];

function zs(name) {
  return table(name, { schema });
}

function pzs(name) {
  return table(name, { schema });
}

function journalTableTarget() {
  return { kind: "table", schema, names: journalTables };
}

export default {
  name: "durable_workflows_journal",
  schema() {
    deploymentSchema(schema);

    zs("workflow_runs").create({
      columns: {
        id: t.text().notNull(),
        workflow_name: t.text().notNull(),
        app_id: t.text().notNull(),
        deploy_id: t.text().notNull(),
        state: t.text().notNull(),
        input: t.json(),
        output: t.json(),
        error: t.json(),
        output_kind: t.text().notNull().default("inline"),
        output_hash: t.char({ length: 64 }),
        output_size: t.bigInt(),
        output_content_type: t.text(),
        input_hash: t.char({ length: 64 }),
        input_size: t.bigInt(),
        input_content_type: t.text(),
        journal_bytes: t.bigInt().notNull().default(0),
        blob_bytes: t.bigInt().notNull().default(0),
        wake_at: t.timestamp(),
        claimed_by: t.text(),
        lease_expires: t.timestamp(),
        dispatch_nonce: t.text(),
        last_dispatch_at: t.timestamp(),
        concurrency: t.smallInt().notNull().default(1),
        next_ordinal: t.int().notNull().default(0),
        stuck_strikes: t.smallInt().notNull().default(0),
        waiting_step_key: t.text(),
        paused_from_status: t.text(),
        signal_epoch: t.int().notNull().default(0),
        parent_run_id: t.text(),
        parent_wait_step_key: t.text(),
        parent_cascade: t.boolean().notNull().default(false),
        tree_depth: t.smallInt().notNull().default(0),
        cancel_requested: t.boolean().notNull().default(false),
        compensation_target: t.text(),
        compensation_outcome: t.text(),
        restart_count: t.smallInt().notNull().default(0),
        restarted_at: t.timestamp(),
        restarted_from_ordinal: t.int(),
        restarted_by: t.text(),
        dedup_key: t.text(),
        started_at: t.timestamp().notNull(),
        terminal_at: t.timestamp(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    zs("workflow_runs").check("workflow_runs_state_check").add({
      expr: (c) => c("state").in(["queued", "running", "sleeping", "waiting", "paused", "stalled", "compensating", "completed", "failed", "cancelled"]),
    });
    zs("workflow_runs").check("workflow_runs_compensation_target_check").add({ expr: (c) => c("compensation_target").in(["failed", "cancelled"]) });
    zs("workflow_runs").check("workflow_runs_compensation_outcome_check").add({ expr: (c) => c("compensation_outcome").in(["completed", "partial"]) });
    pzs("workflow_runs").check("workflow_runs_check").add({
      expr: (c) =>
        c("output_kind")
          .eq("inline")
          .and(c("output_hash").isNull())
          .or(c("output_kind").eq("blob").and(c("output_hash").isNotNull(), c("output_size").isNotNull(), c("output").isNull(), c("output_hash").regex("^[0-9a-f]{64}$"))),
    });
    zs("workflow_runs").check("workflow_runs_check1").add({ expr: (c) => c("input").isNotNull().or(c("input_hash").isNotNull()) });
    zs("workflow_runs").check("workflow_runs_check2").add({ expr: (c) => c("parent_run_id").isNull().eq(c("parent_wait_step_key").isNull()) });
    zs("workflow_runs").unique("workflow_runs_app_id_workflow_name_dedup_key_key").add({ columns: ["app_id", "workflow_name", "dedup_key"] });
    pzs("workflow_runs").index("workflow_runs_wake_due_idx").add({
      on: ["wake_at"],
      where: (c) => c("state").in(["sleeping", "waiting", "compensating"]).and(c("wake_at").isNotNull()),
    });
    pzs("workflow_runs").index("workflow_runs_lease_sweep_idx").add({
      on: ["lease_expires"],
      where: (c) => c("claimed_by").isNotNull(),
    });
    pzs("workflow_runs").index("workflow_runs_live_children_idx").add({
      on: ["parent_run_id"],
      where: (c) => c("parent_run_id").isNotNull().and(c("state").notIn(["completed", "failed", "cancelled", "stalled"])),
    });
    pzs("workflow_runs").index("workflow_runs_cancel_requested_idx").add({
      on: ["id"],
      where: (c) => c("cancel_requested"),
    });
    pzs("workflow_runs").index("workflow_runs_terminal_retention_idx").add({
      on: ["terminal_at", { column: "tree_depth", order: "desc" }, "id"],
      where: (c) => c("state").in(["completed", "failed", "cancelled", "stalled"]).and(c("terminal_at").isNotNull()),
    });

    zs("workflow_blobs").create({
      columns: {
        id: t.bigInt().notNull().identity(),
        hash: t.char({ length: 64 }).notNull(),
        size: t.bigInt().notNull(),
        content_type: t.text().notNull(),
        refcount: t.int().notNull().default(0),
        first_seen_at: t.timestamp().notNull().default(now()),
        last_referenced_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    zs("workflow_blobs").unique("workflow_blobs_natural_key").add({ columns: ["hash"] });
    pzs("workflow_blobs").check("workflow_blobs_hash_check").add({ expr: (c) => c("hash").regex("^[0-9a-f]{64}$") });
    pzs("workflow_blobs").index("workflow_blobs_gc_idx").add({
      on: ["last_referenced_at"],
      where: (c) => c("refcount").eq(0),
    });

    zs("workflow_signal_keys").create({
      columns: {
        id: t.text().notNull(),
        app_id: t.text().notNull(),
        kid: t.text().notNull(),
        verifier: t.text().notNull(),
        secret_ct: t.bytes().notNull(),
        secret_kek: t.text().notNull(),
        status: t.text().notNull().default("active"),
        created_at: t.timestamp().notNull().default(now()),
        rotated_at: t.timestamp(),
        retired_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_signal_keys").check("workflow_signal_keys_verifier_check").add({ expr: (c) => c("verifier").in(["zeroship-hmac", "bearer-signing", "provider:stripe"]) });
    zs("workflow_signal_keys").check("workflow_signal_keys_status_check").add({ expr: (c) => c("status").in(["active", "next", "retiring", "retired"]) });
    zs("workflow_signal_keys").unique("workflow_signal_keys_app_id_kid_key").add({ columns: ["app_id", "kid"] });
    zs("workflow_signal_keys").index("workflow_signal_keys_app_status_idx").add({ on: ["app_id", "status"] });

    zs("workflow_broadcasts").create({
      columns: {
        id: t.text().notNull(),
        app_id: t.text().notNull(),
        topic: t.text().notNull(),
        type: t.text().notNull(),
        payload: t.json().notNull(),
        origin: t.text().notNull(),
        provider: t.text(),
        idempotency_key: t.text().notNull(),
        deploy_id: t.text().notNull(),
        fanout_state: t.text().notNull().default("pending"),
        created_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_broadcasts").check("workflow_broadcasts_origin_check").add({ expr: (c) => c("origin").in(["app", "ingress"]) });
    zs("workflow_broadcasts").check("workflow_broadcasts_fanout_state_check").add({ expr: (c) => c("fanout_state").in(["pending", "completed"]) });
    zs("workflow_broadcasts").unique("workflow_broadcasts_app_id_topic_idempotency_key_key").add({ columns: ["app_id", "topic", "idempotency_key"] });
    pzs("workflow_broadcasts").index("workflow_broadcasts_pending_fanout_idx").add({
      on: ["app_id", "topic", "created_at"],
      where: (c) => c("fanout_state").eq("pending"),
    });
    zs("workflow_broadcasts").index("workflow_broadcasts_expires_idx").add({ on: ["expires_at"] });

    zs("workflow_steps").create({
      columns: {
        id: t.bigInt().notNull().identity(),
        run_id: t.text().notNull(),
        ordinal: t.int().notNull(),
        name: t.text().notNull(),
        name_occurrence: t.int().notNull().default(0),
        kind: t.text().notNull(),
        state: t.text().notNull(),
        attempt: t.int().notNull().default(0),
        max_attempts: t.int().notNull().default(1),
        output: t.json(),
        error: t.json(),
        output_kind: t.text().notNull().default("inline"),
        output_hash: t.char({ length: 64 }),
        output_size: t.bigInt(),
        output_content_type: t.text(),
        wake_at: t.timestamp(),
        signal_type: t.text(),
        max_signal_age_ms: t.bigInt(),
        consumed_signal_id: t.text(),
        child_run_id: t.text(),
        batch_id: t.text().notNull(),
        batch_width: t.smallInt().notNull().default(1),
        started_at: t.timestamp().notNull().default(now()),
        finished_at: t.timestamp(),
        compensation_state: t.text(),
        compensation_attempt: t.int().notNull().default(0),
        compensation_max_attempts: t.int().notNull().default(1),
        compensation_wake_at: t.timestamp(),
        compensation_error: t.json(),
        compensation_batch_id: t.text(),
        compensation_finished_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_steps").unique("workflow_steps_natural_key").add({ columns: ["run_id", "ordinal"] });
    zs("workflow_steps").check("workflow_steps_kind_check").add({ expr: (c) => c("kind").in(["run", "sideEffect", "sleep", "wait_signal", "child"]) });
    zs("workflow_steps").check("workflow_steps_state_check").add({ expr: (c) => c("state").in(["running", "completed", "failed"]) });
    pzs("workflow_steps").check("workflow_steps_check").add({
      expr: (c) =>
        c("output_kind")
          .eq("inline")
          .and(c("output_hash").isNull())
          .or(c("output_kind").eq("blob").and(c("output_hash").isNotNull(), c("output_size").isNotNull(), c("output").isNull(), c("output_hash").regex("^[0-9a-f]{64}$"))),
    });
    zs("workflow_steps").check("workflow_steps_compensation_state_check").add({
      expr: (c) => c("compensation_state").isNull().or(c("compensation_state").in(["pending", "running", "completed", "failed"])),
    });
    zs("workflow_steps").check("workflow_steps_check1").add({ expr: (c) => c("compensation_state").isNull().or(c("kind").eq("run")) });
    zs("workflow_steps").unique("workflow_steps_run_id_name_name_occurrence_key").add({ columns: ["run_id", "name", "name_occurrence"] });
    pzs("workflow_steps").index("workflow_steps_running_wake_idx").add({
      on: ["run_id", "wake_at"],
      where: (c) => c("state").eq("running").and(c("wake_at").isNotNull()),
    });
    pzs("workflow_steps").index("workflow_steps_compensation_frontier_idx").add({
      on: ["run_id", { column: "ordinal", order: "desc" }],
      where: (c) => c("compensation_state").in(["pending", "running"]),
    });
    pzs("workflow_steps").index("workflow_steps_compensation_wake_idx").add({
      on: ["run_id", "compensation_wake_at"],
      where: (c) => c("compensation_state").eq("running").and(c("compensation_wake_at").isNotNull()),
    });

    zs("workflow_signals").create({
      columns: {
        id: t.text().notNull(),
        run_id: t.text().notNull(),
        type: t.text().notNull(),
        payload: t.json(),
        created_at: t.timestamp().notNull().default(now()),
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
    zs("workflow_signals").check("workflow_signals_origin_check").add({ expr: (c) => c("origin").in(["app", "ingress", "system"]) });
    zs("workflow_signals").check("workflow_signals_delivery_check").add({ expr: (c) => c("delivery").in(["direct", "topic"]) });
    zs("workflow_signals").check("workflow_signals_check").add({ expr: (c) => c("delivery").eq("topic").eq(c("topic").isNotNull()) });
    pzs("workflow_signals").index("workflow_signals_pending_idx").add({
      on: ["run_id", "type"],
      where: (c) => c("consumed_by").isNull(),
    });
    pzs("workflow_signals").index("workflow_signals_ext_idem_uidx").add({
      on: ["run_id", "type", "idempotency_key"],
      unique: true,
      where: (c) => c("idempotency_key").isNotNull().and(c("delivery").ne("topic")),
    });
    pzs("workflow_signals").index("workflow_signals_bcast_run_uidx").add({
      on: ["broadcast_id", "run_id"],
      unique: true,
      where: (c) => c("broadcast_id").isNotNull(),
    });

    zs("workflow_subscriptions").create({
      columns: {
        id: t.text().notNull(),
        app_id: t.text().notNull(),
        topic: t.text().notNull(),
        run_id: t.text().notNull(),
        signal_name: t.text().notNull(),
        type_filter: t.text(),
        ordinal: t.int().notNull(),
        max_age_ms: t.bigInt(),
        created_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_subscriptions").unique("workflow_subscriptions_run_id_ordinal_key").add({ columns: ["run_id", "ordinal"] });
    zs("workflow_subscriptions").index("workflow_subscriptions_app_topic_idx").add({ on: ["app_id", "topic"] });

    zs("workflow_schedules").create({
      columns: {
        id: t.text().notNull(),
        app_id: t.text().notNull(),
        deploy_id: t.text().notNull(),
        deploy_hash: t.text().notNull(),
        name: t.text().notNull(),
        workflow_name: t.text().notNull(),
        kind: t.text().notNull(),
        cron_expr: t.text(),
        tz: t.text(),
        interval_ms: t.bigInt(),
        anchor: t.text(),
        input_json: t.json().notNull().default({}),
        overlap: t.text().notNull().default("allow"),
        catch_up: t.text().notNull().default("skip"),
        catch_up_max: t.int().notNull().default(0),
        next_fire_at: t.timestamp().notNull(),
        last_fire_at: t.timestamp(),
        last_fired_epoch: t.bigInt(),
        enabled: t.boolean().notNull().default(true),
        claimed_by: t.text(),
        claimed_at: t.timestamp(),
        lease_expires: t.timestamp(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    zs("workflow_schedules").check("workflow_schedules_kind_check").add({ expr: (c) => c("kind").in(["cron", "interval"]) });
    zs("workflow_schedules").check("workflow_schedules_anchor_check").add({ expr: (c) => c("anchor").in(["epoch", "deploy"]) });
    zs("workflow_schedules").check("workflow_schedules_overlap_check").add({ expr: (c) => c("overlap").in(["allow", "skipIfRunning"]) });
    zs("workflow_schedules").check("workflow_schedules_catch_up_check").add({ expr: (c) => c("catch_up").in(["skip", "backfill"]) });
    zs("workflow_schedules").check("workflow_schedules_catch_up_max_check").add({ expr: (c) => c("catch_up_max").ge(0) });
    zs("workflow_schedules").check("workflow_schedules_check").add({
      expr: (c) =>
        c("kind")
          .eq("cron")
          .and(c("cron_expr").isNotNull(), c("tz").isNotNull(), c("interval_ms").isNull(), c("anchor").isNull())
          .or(c("kind").eq("interval").and(c("interval_ms").isNotNull(), c("anchor").isNotNull(), c("cron_expr").isNull(), c("tz").isNull())),
    });
    zs("workflow_schedules").unique("workflow_schedules_app_id_name_key").add({ columns: ["app_id", "name"] });
    pzs("workflow_schedules").index("workflow_schedules_due_idx").add({
      on: ["next_fire_at"],
      where: (c) => c("enabled"),
    });
    zs("workflow_schedules").index("workflow_schedules_app_idx").add({ on: ["app_id"] });

    zs("workflow_rollout_config").create({
      columns: {
        id: t.text().notNull().default("global"),
        dispatch_paused: t.boolean().notNull().default(false),
        ingress_disabled: t.boolean().notNull().default(false),
        source_validity_ms: t.bigInt().notNull(),
        updated_at: t.timestamp().notNull().default(now()),
        updated_by: t.text(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_rollout_config").check("workflow_rollout_config_id_check").add({ expr: (c) => c("id").eq("global") });
    zs("workflow_rollout_config").check("workflow_rollout_config_validity_check").add({ expr: (c) => c("source_validity_ms").gt(0) });

    zs("workflow_policy_ledger").create({
      columns: {
        id: t.text().notNull(),
        revision: t.bigInt().notNull().default(0),
        policy_json: t.json(),
        source_validity_ms: t.bigInt(),
      },
      primaryKey: ["id"],
    });
    zs("workflow_policy_ledger").check("workflow_policy_ledger_publication_check").add({
      expr: (c) => c("revision").eq(0).and(c("policy_json").isNull(), c("source_validity_ms").isNull())
        .or(c("revision").gt(0).and(c("policy_json").isNotNull(), c("source_validity_ms").isNotNull(), c("source_validity_ms").gt(0))),
    });

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

    grant({ privileges: ["select", "insert", "update", "delete"], on: journalTableTarget(), to: ["zeroship_control"] });
  },
};
