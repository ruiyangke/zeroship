/**
 * Robustness — Gap W: non-deterministic `migrateOne` (flaky external API).
 *
 * The current contract is "no retry; dead-letter on first throw". A
 * `migrateOne` that throws on call #1 and would have succeeded on a
 * retry is dead-lettered immediately. This test pins the contract so
 * that a future "retry support" change can intentionally break this
 * test and reviewers can negotiate the new behaviour explicitly.
 *
 * Concretely: with `failureBudget: 1` and a flaky migrateOne that
 * throws on its first invocation for any given row id, the row
 * appears in `deadLetterPks` and `migrateOne` is not called again for
 * that row in the same run.
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

describe("migrations.run — non-deterministic migrateOne (Gap W contract)", () => {
  test("flaky migrateOne (throws on call 1, succeeds on call 2) is dead-lettered on call 1 with NO retry", async () => {
    const { native, state } = createMockNative({ rows: seedUsers(5) });

    const callCount = new Map<number, number>();
    const m = defineMigration({
      name: "flaky_backfill",
      collection: "users",
      batchSize: 5,
      failureBudget: 5,
      migrateOne: (doc: { id: number }) => {
        const prior = callCount.get(doc.id) ?? 0;
        callCount.set(doc.id, prior + 1);
        if (doc.id === 3 && prior === 0) {
          // Would have succeeded on a hypothetical second call —
          // current SDK never retries, so this row is dead-lettered.
          throw new Error("flaky API hiccup");
        }
        return { role: "user" };
      },
    });

    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.ok(data);
    // Row 3 was dead-lettered.
    assert.deepEqual(data!.deadLetterPks, [3]);
    assert.equal(data!.status, "applied_with_dead_letter");
    // The contract: migrateOne for row 3 was called EXACTLY ONCE — no
    // hidden retry happened underneath. If retry support is added in
    // a follow-up, this assertion must be updated alongside the
    // documented retry policy.
    assert.equal(
      callCount.get(3),
      1,
      "no retry — migrateOne for row 3 must be called exactly once",
    );
    // Other rows applied normally.
    assert.equal(state.rows.find((r) => r.id === 1)!.role, "user");
    assert.equal(state.rows.find((r) => r.id === 2)!.role, "user");
    // Row 3 itself was not mutated (its update was skipped via the
    // dead-letter path).
    assert.equal(state.rows.find((r) => r.id === 3)!.role, null);
    assert.equal(state.rows.find((r) => r.id === 4)!.role, "user");
    assert.equal(state.rows.find((r) => r.id === 5)!.role, "user");
  });

  test("flaky across batches — a row failed in batch 1 stays dead-lettered even if a later sweep would succeed", async () => {
    // Two-batch run; the row that throws in batch 1 is not re-fetched
    // in batch 2 (cursor advanced past it). Pin the "terminal dead-
    // lettering per row" behaviour.
    const { native } = createMockNative({ rows: seedUsers(10) });
    const seen = new Set<number>();
    const m = defineMigration({
      name: "flaky_batched",
      collection: "users",
      batchSize: 5,
      failureBudget: 5,
      migrateOne: (doc: { id: number }) => {
        if (!seen.has(doc.id)) {
          seen.add(doc.id);
          if (doc.id === 7) throw new Error("transient");
        }
        return { role: "user" };
      },
    });
    const { data, error } = await migrations.run(m, {}, native);
    assert.equal(error, null);
    assert.deepEqual(data!.deadLetterPks, [7]);
    // The audit's contract: even though `seen.has(7)` is now true and a
    // re-invocation would succeed, the SDK does not loop back. Status
    // settles as applied_with_dead_letter.
    assert.equal(data!.status, "applied_with_dead_letter");
  });
});
