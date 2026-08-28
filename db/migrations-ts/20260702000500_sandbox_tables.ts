import { table, t, now } from "zero-migrate";

export default {
  name: "sandbox_tables",
  schema() {
    table("deleted_sandboxes", { schema: "zeroship" }).create({
      columns: {
        sandbox_id: t.text().notNull(),
        user_id: t.text().notNull(),
        deleted_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["sandbox_id"],
    });
    table("deleted_sandboxes", { schema: "zeroship" }).check("deleted_sandboxes_sandbox_id_check").add({ expr: (col) => col("sandbox_id").regex("^sbx_[0-9A-Za-z]{20,40}$") });
    table("deleted_sandboxes", { schema: "zeroship" }).check("deleted_sandboxes_user_id_check").add({ expr: (col) => col("user_id").regex("^usr_[0-9A-Za-z]{20,40}$") });
    table("hosts", { schema: "zeroship" }).create({
      columns: {
        host_id: t.text().notNull(),
        boot_id: t.text().notNull(),
        hostname: t.text().notNull(),
        region: t.text().notNull(),
        backend: t.text().notNull(),
        started_at: t.timestamp().notNull().default(now()),
        last_heartbeat: t.timestamp().notNull().default(now()),
        status: t.text().notNull().default("alive"),
        drain_started_at: t.timestamp(),
        version: t.text().notNull().default(""),
        metadata: t.json().notNull().default({}),
      },
      primaryKey: ["host_id"],
    });
    table("hosts", { schema: "zeroship" }).check("hosts_backend_check").add({ expr: (col) => col("backend").in(["docker", "k8s", "nomad-ch"]) });
    table("hosts", { schema: "zeroship" }).check("hosts_boot_id_check").add({ expr: (col) => col("boot_id").regex("^[0-9A-Za-z]{20,40}$") });
    table("hosts", { schema: "zeroship" }).check("hosts_host_id_check").add({ expr: (col) => col("host_id").regex("^hst_[0-9A-Za-z]{20,40}$") });
    table("hosts", { schema: "zeroship" }).check("hosts_region_check").add({ expr: (col) => col("region").regex("^[a-z][a-z0-9-]{1,63}$") });
    table("hosts", { schema: "zeroship" }).check("hosts_status_check").add({ expr: (col) => col("status").in(["alive", "draining", "dead"]) });
    table("sandbox_events", { schema: "zeroship" }).create({
      columns: {
        event_id: t.text().notNull(),
        sandbox_id: t.text(),
        user_id: t.text().notNull(),
        kind: t.text().notNull(),
        ts: t.timestamp().notNull().default(now()),
        data: t.json().notNull().default({}),
      },
      primaryKey: ["ts", "event_id"],
      partitionBy: { range: ["ts"] },
    });
    table("sandbox_events", { schema: "zeroship" }).check("sandbox_events_data_check").add({ expr: (col) => col("data").columnSize().le(8192) });
    table("sandbox_events", { schema: "zeroship" }).check("sandbox_events_event_id_check").add({ expr: (col) => col("event_id").regex("^evt_[0-9A-Za-z]{20,40}$") });
    table("sandbox_events", { schema: "zeroship" }).check("sandbox_events_sandbox_id_check").add({ expr: (col) => col("sandbox_id").regex("^sbx_[0-9A-Za-z]{20,40}$") });
    table("sandbox_events", { schema: "zeroship" }).check("sandbox_events_user_id_check").add({ expr: (col) => col("user_id").regex("^usr_[0-9A-Za-z]{20,40}$") });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_05").create({ from: ["2026-05-01 00:00:00+00"], to: ["2026-06-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_06").create({ from: ["2026-06-01 00:00:00+00"], to: ["2026-07-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_07").create({ from: ["2026-07-01 00:00:00+00"], to: ["2026-08-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_08").create({ from: ["2026-08-01 00:00:00+00"], to: ["2026-09-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_09").create({ from: ["2026-09-01 00:00:00+00"], to: ["2026-10-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_2026_10").create({ from: ["2026-10-01 00:00:00+00"], to: ["2026-11-01 00:00:00+00"] });
    table("sandbox_events", { schema: "zeroship" }).partition("sandbox_events_default").create({ default: true });
    table("sandboxes", { schema: "zeroship" }).create({
      columns: {
        sandbox_id: t.text().notNull(),
        user_id: t.text().notNull(),
        project_id: t.text().notNull(),
        backend: t.text().notNull(),
        vm_index: t.int(),
        agent_url: t.text(),
        host_id: t.text().notNull(),
        generation: t.bigInt().notNull().default(0),
        status: t.text().notNull().default("starting"),
        key_fp: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        started_at: t.timestamp(),
        stopped_at: t.timestamp(),
        last_used_at: t.timestamp().notNull().default(now()),
        deleted_at: t.timestamp(),
        metadata: t.json().notNull().default({}),
        snapshot_artifact_path: t.text(),
        snapshot_taken_at: t.timestamp(),
        snapshot_ch_version: t.text(),
        snapshot_sha256: t.bytes(),
        snapshot_aead_dek_id: t.text(),
        snapshot_backing_versions: t.json(),
        snapshot_vm_index: t.smallInt(),
        lessee_updated_at: t.timestamp(),
        last_running_worker_id: t.text(),
        idle_snapshot_opted_in: t.boolean().notNull().default(false),
        idle_snapshot_count_long_poll: t.boolean().notNull().default(false),
        last_drain_failure_at: t.timestamp(),
        drain_failure_count: t.int().notNull().default(0),
      },
      primaryKey: ["sandbox_id"],
    });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_backend_check").add({ expr: (col) => col("backend").in(["docker", "k8s", "nomad-ch"]) });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_generation_check").add({ expr: (col) => col("generation").ge(0) });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_key_fp_check").add({ expr: (col) => col("key_fp").regex("^[0-9a-f]{32}$") });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_project_id_check").add({ expr: (col) => col("project_id").regex("^prj_[0-9A-Za-z]{20,40}$") });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_sandbox_id_check").add({ expr: (col) => col("sandbox_id").regex("^sbx_[0-9A-Za-z]{20,40}$") });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_snapshot_artifact_consistency").add({ expr: (col) =>
      col("status").notIn(["snapshotted", "snapshotted_suspect"]).or(
        col("snapshot_artifact_path").isNotNull().and(
          col("snapshot_sha256").isNotNull(),
          col("snapshot_ch_version").isNotNull(),
        ),
      ) });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_status_check").add({ expr: (col) => col("status").in(["starting", "running", "stopping", "stopped", "lost", "recreating", "orphan", "unreachable", "snapshotting", "snapshotted", "snapshotting_aborted", "snapshotted_suspect", "restoring", "restoring_cold"]) });
    table("sandboxes", { schema: "zeroship" }).check("sandboxes_user_id_check").add({ expr: (col) => col("user_id").regex("^usr_[0-9A-Za-z]{20,40}$") });
    table("shares", { schema: "zeroship" }).create({
      columns: {
        token_id: t.text().notNull(),
        sandbox_id: t.text().notNull(),
        port: t.int().notNull(),
        scope: t.text().notNull(),
        secret_version: t.int().notNull(),
        issued_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        revoked_at: t.timestamp(),
        use_count: t.bigInt().notNull().default(0),
        last_used_at: t.timestamp(),
        iss: t.text(),
        deleted_at: t.timestamp(),
      },
      primaryKey: ["token_id"],
    });
    table("shares", { schema: "zeroship" }).check("shares_check").add({ expr: (col) => col("expires_at").gt(col("issued_at")) });
    table("shares", { schema: "zeroship" }).check("shares_check1").add({ expr: (col) => col("revoked_at").isNull().or(col("revoked_at").ge(col("issued_at"))) });
    table("shares", { schema: "zeroship" }).check("shares_iss_check").add({ expr: (col) => col("iss").isNull().or(col("iss").regex("^usr_[0-9A-Za-z]{20,40}$")) });
    table("shares", { schema: "zeroship" }).check("shares_port_check").add({ expr: (col) => col("port").ge(1).and(col("port").le(65535)) });
    table("shares", { schema: "zeroship" }).check("shares_scope_check").add({ expr: (col) => col("scope").in(["ro", "rw"]) });
    table("shares", { schema: "zeroship" }).check("shares_secret_version_check").add({ expr: (col) => col("secret_version").ge(1) });
    table("shares", { schema: "zeroship" }).check("shares_token_id_check").add({ expr: (col) => col("token_id").regex("^tok_[A-Za-z0-9_-]{20,40}$") });
    table("wake_jobs", { schema: "zeroship" }).create({
      columns: {
        wake_id: t.text().notNull(),
        sandbox_id: t.text().notNull(),
        state: t.text().notNull(),
        error_code: t.text(),
        error_message: t.text(),
        started_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
        ready_at: t.timestamp(),
        agent_url: t.text(),
        lessee: t.text().notNull(),
        lessee_updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["wake_id"],
    });
    table("wake_jobs", { schema: "zeroship" }).check("wake_jobs_agent_url_chk").add({ expr: (col) => col("agent_url").isNull().or(col("agent_url").regex("^https?://[a-zA-Z0-9._:/-]+$")) });
    table("wake_jobs", { schema: "zeroship" }).check("wake_jobs_error_code_check").add({ expr: (col) => col("error_code").isNull().or(col("error_code").in(["slot_unavailable", "source_teardown_timeout", "restore_failed", "livez_timeout", "clock_resync_failed", "register_failed", "internal", "wake_worker_aborted", "staging_path_missing", "agent_version_mismatch"])) });
    table("wake_jobs", { schema: "zeroship" }).check("wake_jobs_sandbox_id_check").add({ expr: (col) => col("sandbox_id").regex("^sbx_[0-9A-Za-z]{20,40}$") });
    table("wake_jobs", { schema: "zeroship" }).check("wake_jobs_state_check").add({ expr: (col) => col("state").in(["pending", "reserving_slot", "restoring", "livez_polling", "clock_resyncing", "registering", "ok", "failed"]) });
  },
};
