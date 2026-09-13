import { dialect, raw, table, t } from "../../../packages/zero-migrate/dist/index.js";

// Keep identity columns and their foreign-key copies bytewise, following the
// platform's sortable_entity_id_collations migration convention.
const identityColumns = {
  queue_scopes: ["id", "app_id"],
  jobs: ["id", "app_id", "deployment_id", "worker_id"],
};

// Queue records carry closed job metadata; customer history and payloads stay
// in the worker's creator database and object storage.
export function workflowManagerSchema(namespace) {
  const text = () => t.text().notNull();
  const integer = () => t.bigInt().notNull();
  const scopes = table("queue_scopes", { schema: namespace });
  scopes.create({
    columns: {
      id: text(),
      app_id: text(),
      lock_version: integer().default(0),
    },
    primaryKey: ["id"],
  });
  scopes.index("queue_scopes_app_id_key").add({ on: ["app_id"], unique: true });

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
      references: { schema: namespace, table: "queue_scopes", columns: ["app_id"] },
      onDelete: "restrict",
    }],
  });
  jobs.index("jobs_available_idx").add({ on: ["app_id", "state", "available_at", "id"] });
  jobs.index("jobs_lease_idx").add({ on: ["app_id", "state", "lease_deadline", "id"] });

  // ColumnDef does not yet expose the engine's portable bytewise collation
  // facet. Keep this PostgreSQL-specific DDL in the migration recorder, where
  // the compiler authorizes and emits it. SQLite text defaults to BINARY.
  dialect({
    postgres() {
      for (const [name, columns] of Object.entries(identityColumns)) {
        raw({
          sql: `ALTER TABLE "${namespace}"."${name}" ${columns.map(column => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`).join(", ")}`,
          reason: "job identities and their copies require bytewise comparison",
        });
      }
    },
    sqlite() {},
  });
}
