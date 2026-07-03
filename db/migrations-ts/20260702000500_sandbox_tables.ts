import { and, membership, notMembership, or, table, t } from "@zeroship/migrate";
import { raw } from "@zeroship/migrate/pg";

export const name = "sandbox_tables";

export function up() {
  table("deleted_sandboxes", { schema: "zeroship" }).create({
    columns: {
      sandbox_id: t.text().notNull(),
      user_id: t.text().notNull(),
      deleted_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["sandbox_id"],
  });
  table("deleted_sandboxes", { schema: "zeroship" }).addCheck("deleted_sandboxes_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("deleted_sandboxes", { schema: "zeroship" }).addCheck("deleted_sandboxes_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("hosts", { schema: "zeroship" }).create({
    columns: {
      host_id: t.text().notNull(),
      boot_id: t.text().notNull(),
      hostname: t.text().notNull(),
      region: t.text().notNull(),
      backend: t.text().notNull(),
      started_at: t.timestamp().notNull().default({ fn: "now" }),
      last_heartbeat: t.timestamp().notNull().default({ fn: "now" }),
      status: t.text().notNull().default("alive"),
      drain_started_at: t.timestamp(),
      version: t.text().notNull().default(""),
      metadata: t.json().notNull(),
    },
    primaryKey: ["host_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ALTER COLUMN metadata SET DEFAULT '{}'::jsonb", reason: "column hosts.metadata requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("hosts", { schema: "zeroship" }).addCheck("hosts_backend_check", (c) => membership(c("backend"), ["docker", "k8s", "nomad-ch"]));
  table("hosts", { schema: "zeroship" }).addCheck("hosts_boot_id_check", (c) => c("boot_id").matches("^[0-9A-Za-z]{20,40}$"));
  table("hosts", { schema: "zeroship" }).addCheck("hosts_host_id_check", (c) => c("host_id").matches("^hst_[0-9A-Za-z]{20,40}$"));
  table("hosts", { schema: "zeroship" }).addCheck("hosts_region_check", (c) => c("region").matches("^[a-z][a-z0-9-]{1,63}$"));
  table("hosts", { schema: "zeroship" }).addCheck("hosts_status_check", (c) => membership(c("status"), ["alive", "draining", "dead"]));
  // TODO(dsl-v2): add structural partitioned table support for exact platform tables
  raw({ sql: "CREATE TABLE zeroship.sandbox_events (\n    event_id text NOT NULL,\n    sandbox_id text,\n    user_id text NOT NULL,\n    kind text NOT NULL,\n    ts timestamp with time zone DEFAULT now() NOT NULL,\n    data jsonb DEFAULT '{}'::jsonb NOT NULL,\n    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),\n    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),\n    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),\n    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))\n)\nPARTITION BY RANGE (ts)", reason: "partitioned table zeroship.sandbox_events uses PARTITION BY RANGE (ts), which createTable cannot express yet" });
  table("sandbox_events_2026_05", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_05.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_05", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_05", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_05", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_05", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_06", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_06.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_06", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_06", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_06", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_06", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_07", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_07.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_07", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_07", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_07", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_07", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_08", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_08.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_08", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_08", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_08", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_08", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_09", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_09.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_09", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_09", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_09", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_09", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_10", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10 ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_2026_10.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_2026_10", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_2026_10", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_10", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_2026_10", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_default", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      sandbox_id: t.text(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      ts: t.timestamp().notNull().default({ fn: "now" }),
      data: t.json().notNull(),
    },
    primaryKey: ["ts", "event_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default ALTER COLUMN data SET DEFAULT '{}'::jsonb", reason: "column sandbox_events_default.data requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandbox_events_default", { schema: "zeroship" }).addCheck("sandbox_events_data_check", (c) => c("data").columnSize().le(8192));
  table("sandbox_events_default", { schema: "zeroship" }).addCheck("sandbox_events_event_id_check", (c) => c("event_id").matches("^evt_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_default", { schema: "zeroship" }).addCheck("sandbox_events_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandbox_events_default", { schema: "zeroship" }).addCheck("sandbox_events_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("sandboxes", { schema: "zeroship" }).create({
    columns: {
      sandbox_id: t.text().notNull(),
      user_id: t.text().notNull(),
      project_id: t.text().notNull(),
      backend: t.text().notNull(),
      vm_index: t.integer(),
      agent_url: t.text(),
      host_id: t.text().notNull(),
      generation: t.bigInt().notNull().default(0),
      status: t.text().notNull().default("starting"),
      key_fp: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      started_at: t.timestamp(),
      stopped_at: t.timestamp(),
      last_used_at: t.timestamp().notNull().default({ fn: "now" }),
      deleted_at: t.timestamp(),
      metadata: t.json().notNull(),
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
      drain_failure_count: t.integer().notNull().default(0),
    },
    primaryKey: ["sandbox_id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ALTER COLUMN metadata SET DEFAULT '{}'::jsonb", reason: "column sandboxes.metadata requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_backend_check", (c) => membership(c("backend"), ["docker", "k8s", "nomad-ch"]));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_generation_check", (c) => c("generation").ge(0));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_key_fp_check", (c) => c("key_fp").matches("^[0-9a-f]{32}$"));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_project_id_check", (c) => c("project_id").matches("^prj_[0-9A-Za-z]{20,40}$"));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_snapshot_artifact_consistency", (c) =>
    or(
      notMembership(c("status"), ["snapshotted", "snapshotted_suspect"]),
      and(
        c("snapshot_artifact_path").isNotNull(),
        c("snapshot_sha256").isNotNull(),
        c("snapshot_ch_version").isNotNull(),
      ),
    ));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_status_check", (c) => membership(c("status"), ["starting", "running", "stopping", "stopped", "lost", "recreating", "orphan", "unreachable", "snapshotting", "snapshotted", "snapshotting_aborted", "snapshotted_suspect", "restoring", "restoring_cold"]));
  table("sandboxes", { schema: "zeroship" }).addCheck("sandboxes_user_id_check", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$"));
  table("shares", { schema: "zeroship" }).create({
    columns: {
      token_id: t.text().notNull(),
      sandbox_id: t.text().notNull(),
      port: t.integer().notNull(),
      scope: t.text().notNull(),
      secret_version: t.integer().notNull(),
      issued_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      revoked_at: t.timestamp(),
      use_count: t.bigInt().notNull().default(0),
      last_used_at: t.timestamp(),
      iss: t.text(),
      deleted_at: t.timestamp(),
    },
    primaryKey: ["token_id"],
  });
  table("shares", { schema: "zeroship" }).addCheck("shares_check", (c) => c("expires_at").gt(c("issued_at")));
  table("shares", { schema: "zeroship" }).addCheck("shares_check1", (c) => or(c("revoked_at").isNull(), c("revoked_at").ge(c("issued_at"))));
  table("shares", { schema: "zeroship" }).addCheck("shares_iss_check", (c) => or(c("iss").isNull(), c("iss").matches("^usr_[0-9A-Za-z]{20,40}$")));
  table("shares", { schema: "zeroship" }).addCheck("shares_port_check", (c) => and(c("port").ge(1), c("port").le(65535)));
  table("shares", { schema: "zeroship" }).addCheck("shares_scope_check", (c) => membership(c("scope"), ["ro", "rw"]));
  table("shares", { schema: "zeroship" }).addCheck("shares_secret_version_check", (c) => c("secret_version").ge(1));
  table("shares", { schema: "zeroship" }).addCheck("shares_token_id_check", (c) => c("token_id").matches("^tok_[A-Za-z0-9_-]{20,40}$"));
  table("wake_jobs", { schema: "zeroship" }).create({
    columns: {
      wake_id: t.text().notNull(),
      sandbox_id: t.text().notNull(),
      state: t.text().notNull(),
      error_code: t.text(),
      error_message: t.text(),
      started_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      ready_at: t.timestamp(),
      agent_url: t.text(),
      lessee: t.text().notNull(),
      lessee_updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["wake_id"],
  });
  table("wake_jobs", { schema: "zeroship" }).addCheck("wake_jobs_agent_url_chk", (c) => or(c("agent_url").isNull(), c("agent_url").matches("^https?://[a-zA-Z0-9._:/-]+$")));
  table("wake_jobs", { schema: "zeroship" }).addCheck("wake_jobs_error_code_check", (c) => or(c("error_code").isNull(), membership(c("error_code"), ["slot_unavailable", "source_teardown_timeout", "restore_failed", "livez_timeout", "clock_resync_failed", "register_failed", "internal", "wake_worker_aborted", "staging_path_missing", "agent_version_mismatch"])));
  table("wake_jobs", { schema: "zeroship" }).addCheck("wake_jobs_sandbox_id_check", (c) => c("sandbox_id").matches("^sbx_[0-9A-Za-z]{20,40}$"));
  table("wake_jobs", { schema: "zeroship" }).addCheck("wake_jobs_state_check", (c) => membership(c("state"), ["pending", "reserving_slot", "restoring", "livez_polling", "clock_resyncing", "registering", "ok", "failed"]));
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_05 FOR VALUES FROM ('2026-05-01 00:00:00+00') TO ('2026-06-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_06 FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_07 FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_08 FOR VALUES FROM ('2026-08-01 00:00:00+00') TO ('2026-09-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_09 FOR VALUES FROM ('2026-09-01 00:00:00+00') TO ('2026-10-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_10 FOR VALUES FROM ('2026-10-01 00:00:00+00') TO ('2026-11-01 00:00:00+00')", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_default DEFAULT", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
}

export function down() {

}
