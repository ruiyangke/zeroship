import { table, t } from "@zeroship/migrate";
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
  // TODO(dsl-v2): CHECK constraint deleted_sandboxes.deleted_sandboxes_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.deleted_sandboxes ADD CONSTRAINT deleted_sandboxes_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint deleted_sandboxes.deleted_sandboxes_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.deleted_sandboxes ADD CONSTRAINT deleted_sandboxes_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint hosts.hosts_backend_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ADD CONSTRAINT hosts_backend_check CHECK ((backend = ANY (ARRAY['docker'::text, 'k8s'::text, 'nomad-ch'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint hosts.hosts_boot_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ADD CONSTRAINT hosts_boot_id_check CHECK ((boot_id ~ '^[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint hosts.hosts_host_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ADD CONSTRAINT hosts_host_id_check CHECK ((host_id ~ '^hst_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint hosts.hosts_region_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ADD CONSTRAINT hosts_region_check CHECK ((region ~ '^[a-z][a-z0-9-]{1,63}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint hosts.hosts_status_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.hosts ADD CONSTRAINT hosts_status_check CHECK ((status = ANY (ARRAY['alive'::text, 'draining'::text, 'dead'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_05.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_05.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_05.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_05.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_05 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_06.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_06.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_06.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_06.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_06 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_07.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_07.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_07.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_07.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_07 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_08.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_08.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_08.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_08.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_08 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_09.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_09.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_09.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_09.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_09 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_10.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10 ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_10.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10 ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_10.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10 ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_2026_10.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_2026_10 ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint sandbox_events_default.sandbox_events_data_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default ADD CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_default.sandbox_events_event_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default ADD CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_default.sandbox_events_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default ADD CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandbox_events_default.sandbox_events_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandbox_events_default ADD CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
      snapshot_vm_index: t.integer(),
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
  // TODO(dsl-v2): add structural column type support for smallint
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ALTER COLUMN snapshot_vm_index TYPE smallint USING snapshot_vm_index::smallint", reason: "column sandboxes.snapshot_vm_index uses PostgreSQL type smallint, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_backend_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_backend_check CHECK ((backend = ANY (ARRAY['docker'::text, 'k8s'::text, 'nomad-ch'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_generation_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_generation_check CHECK ((generation >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_key_fp_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_key_fp_check CHECK ((key_fp ~ '^[0-9a-f]{32}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_project_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_project_id_check CHECK ((project_id ~ '^prj_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_snapshot_artifact_consistency needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_snapshot_artifact_consistency CHECK (((status <> ALL (ARRAY['snapshotted'::text, 'snapshotted_suspect'::text])) OR ((snapshot_artifact_path IS NOT NULL) AND (snapshot_sha256 IS NOT NULL) AND (snapshot_ch_version IS NOT NULL))))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_status_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_status_check CHECK ((status = ANY (ARRAY['starting'::text, 'running'::text, 'stopping'::text, 'stopped'::text, 'lost'::text, 'recreating'::text, 'orphan'::text, 'unreachable'::text, 'snapshotting'::text, 'snapshotted'::text, 'snapshotting_aborted'::text, 'snapshotted_suspect'::text, 'restoring'::text, 'restoring_cold'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint sandboxes.sandboxes_user_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.sandboxes ADD CONSTRAINT sandboxes_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint shares.shares_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_check CHECK ((expires_at > issued_at))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_check1 needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_check1 CHECK (((revoked_at IS NULL) OR (revoked_at >= issued_at)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_iss_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_iss_check CHECK (((iss IS NULL) OR (iss ~ '^usr_[0-9A-Za-z]{20,40}$'::text)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_port_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_port_check CHECK (((port >= 1) AND (port <= 65535)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_scope_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_scope_check CHECK ((scope = ANY (ARRAY['ro'::text, 'rw'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_secret_version_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_secret_version_check CHECK ((secret_version >= 1))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint shares.shares_token_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.shares ADD CONSTRAINT shares_token_id_check CHECK ((token_id ~ '^tok_[A-Za-z0-9_-]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
  // TODO(dsl-v2): CHECK constraint wake_jobs.wake_jobs_agent_url_chk needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.wake_jobs ADD CONSTRAINT wake_jobs_agent_url_chk CHECK (((agent_url IS NULL) OR (agent_url ~ '^https?://[a-zA-Z0-9._:/-]+$'::text)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint wake_jobs.wake_jobs_error_code_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.wake_jobs ADD CONSTRAINT wake_jobs_error_code_check CHECK (((error_code IS NULL) OR (error_code = ANY (ARRAY['slot_unavailable'::text, 'source_teardown_timeout'::text, 'restore_failed'::text, 'livez_timeout'::text, 'clock_resync_failed'::text, 'register_failed'::text, 'internal'::text, 'wake_worker_aborted'::text, 'staging_path_missing'::text, 'agent_version_mismatch'::text]))))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint wake_jobs.wake_jobs_sandbox_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.wake_jobs ADD CONSTRAINT wake_jobs_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint wake_jobs.wake_jobs_state_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.wake_jobs ADD CONSTRAINT wake_jobs_state_check CHECK ((state = ANY (ARRAY['pending'::text, 'reserving_slot'::text, 'restoring'::text, 'livez_polling'::text, 'clock_resyncing'::text, 'registering'::text, 'ok'::text, 'failed'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
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
