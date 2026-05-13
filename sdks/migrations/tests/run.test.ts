/**
 * Unit tests for `migrations.run` orchestration loop.
 *
 * Uses the in-memory mock native bridge (`mock-native.ts`) so the
 * full state machine can be exercised without a Postgres instance.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { defineMigration } from "../src/define.js";
import { migrations } from "../src/index.js";
import { createMockNative } from "./mock-native.ts";

function seedUsers(n: number): Array<{ id: number; name: string; role: string | null }> {
  const rows: Array<{ id: number; name: string; role: string | null }> = [];
  for (let i = 1; i <= n; i++) {
    rows.push({ id: i, name: `u-${i}`, role: null });
  }
  return rows;
}

describe("migrations.run — happy path", () => {
  test("backfills every row in a single sweep", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(250) });
    const m = defineMigration({
      name: "backfill_role",
      collection: "users",
      batchSize: 100,
      migrateOne: (doc: { role: string | null }) => {
        if (doc.role === null) return { role: "user" };
        return undefined;
      },
    });

    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.equal(data?.status, "applied");
    assert.equal(data?.processed, 250);
    assert.deepEqual(data?.deadLetterPks, []);
    assert.equal(
      state.rows.every((r) => r.role === "user"),
      true,
      "every row should have role='user' after the run",
    );
    // Audit row landed on `applied`.
    assert.equal(state.audit.status, "applied");
    assert.equal(state.audit.processed, 250);
  });

  test("calls fetch+commit one extra time at end to drive the terminal commit", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(50) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 25,
      migrateOne: () => ({ role: "user" }),
    });
    await migrations.run(m, {}, native);
    const fetchCalls = state.calls.filter((c) => c.method === "migrationFetchBatch").length;
    // 50/25 = 2 productive fetches + 1 empty fetch that triggers terminal commit
    assert.equal(fetchCalls, 3);
    // commitBatch: 2 mid-run commits + 1 terminal commit
    const commitCalls = state.calls.filter((c) => c.method === "migrationCommitBatch").length;
    assert.equal(commitCalls, 3);
    // Last commit must have isDone=true.
    const last = state.calls.filter((c) => c.method === "migrationCommitBatch").pop()!;
    assert.equal(last.args[4], true, "last commit must be isDone=true");
    assert.equal(last.args[5], "applied", "terminal status applied");
  });
});

describe("migrations.run — dryRun", () => {
  test("dry-run does not mutate rows", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(20) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 10,
      migrateOne: () => ({ role: "user" }),
    });
    const { data, error } = await migrations.run(m, { dryRun: true }, native);
    assert.equal(error, null);
    assert.equal(data?.status, "applied");
    assert.equal(state.rows.every((r) => r.role === null), true, "dry-run must not mutate");
    // begin was called with dryRun=true
    const begin = state.calls.find((c) => c.method === "migrationBegin")!;
    assert.equal(begin.args[2], true);
  });
});

describe("migrations.run — dead-letter", () => {
  test("rows that throw under failureBudget are dead-lettered", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(10) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 5,
      failureBudget: 2,
      migrateOne: (doc: { id: number }) => {
        if (doc.id === 3) throw new Error("bad row");
        return { role: "user" };
      },
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.equal(data?.status, "applied_with_dead_letter");
    assert.deepEqual(data?.deadLetterPks, [3]);
  });

  test("rows where migrateOne returns null are dead-lettered without counting against budget", async () => {
    const { native } = createMockNative({ rows: seedUsers(10) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 10,
      failureBudget: 0,
      migrateOne: (doc: { id: number }) => {
        if (doc.id === 7) return null; // explicit dead-letter
        return { role: "user" };
      },
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.equal(data?.status, "applied_with_dead_letter");
    assert.deepEqual(data?.deadLetterPks, [7]);
  });

  test("exceeding the failure budget terminates with status=failed", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(10) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 10,
      failureBudget: 1,
      migrateOne: () => {
        throw new Error("always fails");
      },
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.equal(data?.status, "failed");
    assert.equal(state.audit.status, "failed");
    assert.match(state.audit.error ?? "", /migration_failure_budget_exceeded/);
  });
});

describe("migrations.run — cancellation", () => {
  test("a cancel mid-run propagates as cancelled status", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(100) });
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 10,
      migrateOne: () => ({ role: "user" }),
    });
    // Cancel after the first batch is committed by sneaking in via the
    // commit hook — we cancel between batches by flipping the state.
    let batchCount = 0;
    const wrapped = {
      ...native,
      async migrationCommitBatch(...args: Parameters<typeof native.migrationCommitBatch>) {
        const result = await native.migrationCommitBatch(...args);
        batchCount++;
        if (batchCount === 1) state.cancelled = true;
        return result;
      },
    };
    const { data, error } = await migrations.run(m, {}, wrapped);
    assert.equal(error, null);
    assert.equal(data?.status, "cancelled");
  });
});

describe("migrations.run — resume", () => {
  test("begin echoing a non-zero cursor causes the loop to resume", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(20) });
    // Seed mid-run state: audit row pretends rows 1-10 already done.
    state.audit = {
      ...state.audit,
      cursor: 10,
      processed: 10,
      status: "running",
      exists: true,
    };
    // Mark first 10 rows already migrated so the assertion below holds.
    for (const row of state.rows) {
      if (row.id <= 10) row.role = "user";
    }
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 5,
      migrateOne: () => ({ role: "user" }),
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.equal(data?.status, "applied");
    // processed reflects total = pre-existing 10 + 10 new = 20.
    assert.equal(data?.processed, 20);
    // First fetch must have used cursor=10.
    const firstFetch = state.calls.find((c) => c.method === "migrationFetchBatch")!;
    assert.equal(firstFetch.args[0], 10);
  });
});

describe("migrations.run — reset", () => {
  test("reset:true clears the persisted cursor on begin", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(5) });
    // Pre-populate so we can see reset wipes it.
    state.audit = {
      ...state.audit,
      cursor: 4,
      processed: 4,
      status: "failed",
      exists: true,
      error: "old run",
    };
    const m = defineMigration({
      name: "x",
      collection: "users",
      batchSize: 5,
      migrateOne: () => ({ role: "user" }),
    });
    const { error } = await migrations.run(m, { reset: true }, native);
    assert.equal(error, null);
    const beginCall = state.calls.find((c) => c.method === "migrationBegin")!;
    assert.equal(beginCall.args[3], true, "reset arg propagated");
    // First fetch should have used cursor=0 (reset cleared it).
    const firstFetch = state.calls.find((c) => c.method === "migrationFetchBatch")!;
    assert.equal(firstFetch.args[0], 0);
  });
});

describe("migrations.run — advisory lock collision", () => {
  test("migration_already_running surfaces as a Result.error", async () => {
    const { native } = createMockNative({ rows: seedUsers(5), alreadyRunning: true });
    const m = defineMigration({
      name: "x",
      collection: "users",
      migrateOne: () => ({ role: "user" }),
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(data, null);
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "migration_already_running");
  });
});

describe("migrations.status / .cancel / .reset", () => {
  test("status reports the audit-row snapshot", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(3) });
    state.audit = {
      ...state.audit,
      status: "running",
      cursor: 2,
      processed: 2,
      deadLetterPks: [99],
      exists: true,
    };
    const m = defineMigration({
      name: "x",
      collection: "users",
      migrateOne: () => undefined,
    });
    const { data, error } = await migrations.status(m, native);
    assert.equal(error, null);
    assert.equal(data?.status, "running");
    assert.equal(data?.cursor, 2);
    assert.equal(data?.processed, 2);
    assert.deepEqual(data?.deadLetterPks, [99]);
    assert.equal(data?.isDone, false);
  });

  test("cancel transitions a running migration to cancelled", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(3) });
    state.audit.status = "running";
    state.audit.exists = true;
    const m = defineMigration({
      name: "x",
      collection: "users",
      migrateOne: () => undefined,
    });
    const { data, error } = await migrations.cancel(m, native);
    assert.equal(error, null);
    assert.equal(data?.ok, true);
    assert.equal(state.audit.status, "cancelled");
  });

  test("cancel against an applied migration returns migration_not_cancellable", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(3) });
    state.audit.status = "applied";
    state.audit.exists = true;
    const m = defineMigration({
      name: "x",
      collection: "users",
      migrateOne: () => undefined,
    });
    const { data, error } = await migrations.cancel(m, native);
    assert.equal(data, null);
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "migration_not_cancellable");
  });

  test("reset clears persisted state", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(3) });
    state.audit = {
      ...state.audit,
      status: "failed",
      cursor: 2,
      processed: 2,
      error: "old run",
      exists: true,
    };
    const m = defineMigration({
      name: "x",
      collection: "users",
      migrateOne: () => undefined,
    });
    const { error } = await migrations.reset(m, native);
    assert.equal(error, null);
    assert.equal(state.audit.status, "pending");
    assert.equal(state.audit.cursor, 0);
    assert.equal(state.audit.processed, 0);
  });
});
