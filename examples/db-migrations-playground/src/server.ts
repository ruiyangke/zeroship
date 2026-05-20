"use server";

// db-migrations-playground — focused demo of @zeroship/migrations (B1).
//
// Three migrations illustrating the expand-migrate-contract pattern:
//   1. backfillSeverity — additive backfill (NULL → "info")
//   2. expandKind        — rename `event_type` → `kind`
//   3. addUserHash       — derived field from existing data
//
// All three are batched + resumable + dry-runnable + cancellable.
// State lives in __zeroship_migrations (A3 audit table); a crash
// mid-migration is recoverable from `validate_cursor`.

import { createDb, t, schema } from "@zeroship/db";
import { defineMigration, migrations } from "@zeroship/migrations";
import { action, mutation, query } from "@zeroship/server";

// ---------------------------------------------------------------------------
// Schema — represents the "post-expand" shape. Both old and new fields
// are present + nullable so reads see whichever shape exists during the
// migration window.
// ---------------------------------------------------------------------------

export const db = createDb({
  events: schema({
    // Pre-expand: `event_type` was the only kind discriminator.
    // Mid-expand: both `event_type` and `kind` exist; new writes set
    //   both, old rows still have only event_type.
    // Post-contract: drop event_type; kind is the canonical name.
    event_type: t.string(),
    kind:       t.string(),
    severity:   t.string().enum("info", "warn", "error"),
    user_id:    t.number(),
    user_hash:  t.string(),
    payload:    t.json(),
  }),
});

// ---------------------------------------------------------------------------
// 1. backfillSeverity — set severity="info" where null. The simplest
//    case: any row with a null severity gets the default.
// ---------------------------------------------------------------------------

export const backfillSeverity = defineMigration({
  name: "events.backfill_severity",
  collection: "events",
  batchSize: 100,
  migrateOne: async (doc) => {
    if (doc.severity == null) {
      return { severity: "info" };
    }
    return undefined; // skip — already migrated
  },
});

// ---------------------------------------------------------------------------
// 2. expandKind — copy event_type → kind for any row missing kind.
//    Part of the expand-migrate-contract: ship a deploy that writes
//    both fields, run this to backfill, then in a later deploy drop
//    event_type from the schema.
// ---------------------------------------------------------------------------

export const expandKind = defineMigration({
  name: "events.expand_kind",
  collection: "events",
  batchSize: 200,
  migrateOne: async (doc) => {
    if ((doc.kind == null || doc.kind === "") && typeof doc.event_type === "string") {
      return { kind: doc.event_type };
    }
    return undefined;
  },
});

// ---------------------------------------------------------------------------
// 3. addUserHash — derive a sha256-ish hash from user_id for analytics.
//    This is the "derived field" pattern: the new column's value depends
//    on existing column values, so a backfill (not just a default)
//    is required.
// ---------------------------------------------------------------------------

function pseudoHash(n: number): string {
  // Cheap deterministic synth — real code would use crypto.subtle.digest.
  const x = (n * 2654435761) >>> 0;
  return x.toString(16).padStart(8, "0");
}

export const addUserHash = defineMigration({
  name: "events.add_user_hash",
  collection: "events",
  batchSize: 500,
  migrateOne: async (doc) => {
    if ((doc.user_hash == null || doc.user_hash === "") && typeof doc.user_id === "number") {
      return { user_hash: pseudoHash(doc.user_id) };
    }
    return undefined;
  },
});

// ---------------------------------------------------------------------------
// Procedures to exercise the lifecycle from a smoke script
// ---------------------------------------------------------------------------

export const seedEvents = mutation(
  async (
    { count, includeNullSeverity }: { count: number; includeNullSeverity: boolean },
  ) => {
    const col = db.events;
    // Cast through `unknown` because seedEvents deliberately writes
    // `severity: null` to exercise the backfill migration. The schema
    // types severity as `string | undefined`; null is rejected by the
    // typed API but allowed by the underlying column.
    type Row = Parameters<typeof col.insertMany>[0][number];
    const rows: Row[] = [];
    for (let i = 0; i < count; i++) {
      rows.push({
        event_type: i % 2 === 0 ? "login" : "logout",
        kind:       "",
        severity:   (includeNullSeverity && i % 3 === 0 ? null : "info") as unknown as string,
        user_id:    1000 + i,
        user_hash:  "",
        payload:    { i },
      } as Row);
    }
    return col.insertMany(rows);
  },
);

export const eventCount = query(async (_input: Record<string, never>) => {
  return db.events.count({});
});

export const eventStats = query(async (_input: Record<string, never>) => {
  const col = db.events;
  // Unwrap each Result so the smoke can grep `"total":N` directly.
  const total = (await col.count({})).data ?? 0;
  const nullSeverity = (await col.count({ severity: null })).data ?? 0;
  const emptyKind = (await col.count({ kind: "" })).data ?? 0;
  const emptyHash = (await col.count({ user_hash: "" })).data ?? 0;
  return { total, nullSeverity, emptyKind, emptyHash };
});

// ---------------------------------------------------------------------------
// Migration drivers — thin RPC wrappers around `@zeroship/migrations`
// so the smoke can `POST /_zs/v1/runMigration` etc. without depending
// on a control-plane HTTP endpoint that doesn't exist in the dev
// runtime.
// ---------------------------------------------------------------------------

const byName: Record<string, ReturnType<typeof defineMigration>> = {
  "events.backfill_severity": backfillSeverity,
  "events.expand_kind":        expandKind,
  "events.add_user_hash":      addUserHash,
};

function lookup(name: string) {
  const m = byName[name];
  if (!m) throw new Error(`unknown migration: ${name}`);
  return m;
}

// `action` (not `mutation`): the migration opens its own dedicated
// session-scoped advisory-lock connection, so it must not run inside
// the dispatcher's auto-tx — that would put the user-tx connection
// and the migration's lock connection in two different windows on
// the same Postgres backend, deadlocking pglite-socket's single-
// instance serializer. Real long-running migrations don't belong in
// a request-scope tx anyway.
export const runMigration = action(
  async ({ name, dryRun }: { name: string; dryRun?: boolean }) => {
    const m = lookup(name);
    const result = await migrations.run(m, dryRun ? { dryRun: true } : undefined);
    return result.data ?? { error: result.error?.message ?? "unknown" };
  },
);

// `action` (not `query`): `migrations.status` calls
// `env.db.migrations.status({name, collection})` which uses its own
// pooled connection. Under pglite-socket's per-tx serializer, a
// read-only auto-tx would still pin the user-side connection and
// stall the pool query. Status reads aren't tx-scoped reads anyway.
export const migrationStatus = action(
  async ({ name }: { name: string }) => {
    const m = lookup(name);
    const result = await migrations.status(m);
    return result.data ?? { error: result.error?.message ?? "unknown" };
  },
);
