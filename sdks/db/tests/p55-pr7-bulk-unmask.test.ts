/**
 * **P5.5 PR 7** — bulk unmask SDK surface.
 *
 * Pins the JS-side dispatch shape the runtime depends on:
 *
 *   1. `MaskedValue.unmask(columns, opts)` fans out to
 *      `env.db.bulkUnmaskFields` pinned to the MaskedValue's row.
 *   2. `Collection.bulkUnmask([{id, columns}], opts)` builds the
 *      correct wire payload and maps results back into the SDK's
 *      field-name space via `_toField`.
 *   3. `bulk_unmask_partial_unauthorized` thrown by the native side
 *      surfaces with the typed `.code` (Result envelope on
 *      `Collection.bulkUnmask`; raw throw on `MaskedValue.unmask`).
 *
 * The tests stub `env.db.bulkUnmaskFields` directly — we're pinning
 * the SDK's wire shape, not the native dispatcher (the native side is
 * covered by `crates/plugin-db/tests/sqlite_integration.rs`).
 */

import { test, describe, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";

import { MaskedValue } from "../src/types.js";

// Module-global typed handle for the stub. Each test installs a
// fresh `env.db` shape onto `globalThis` and clears it on teardown.
type BulkArgs = {
  collection: string;
  items: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>;
  actor?: unknown;
  reason?: string;
};
type BulkResult = { results: Record<string, Record<string, string>> };

let lastBulkArgs: BulkArgs | null = null;
let bulkStub: ((args: BulkArgs) => Promise<BulkResult>) | null = null;

function installStub(handler: (args: BulkArgs) => Promise<BulkResult>) {
  bulkStub = handler;
  (globalThis as { env?: { db?: Record<string, unknown> } }).env = {
    db: {
      bulkUnmaskFields: async (args: BulkArgs) => {
        lastBulkArgs = args;
        return handler(args);
      },
      collection: (_name: string) => {
        throw new Error("collection() stub not used in PR 7 bulk tests");
      },
    },
  };
}

beforeEach(() => {
  lastBulkArgs = null;
  bulkStub = null;
});

afterEach(() => {
  delete (globalThis as { env?: unknown }).env;
});

describe("P5.5 PR 7 — MaskedValue.unmask(columns, opts) fan-out", () => {
  test("multi-column overload routes through bulkUnmaskFields with the right row pin", async () => {
    installStub(async () => ({
      results: {
        usr_01: {
          ssn: "123-45-6789",
          email: "alice@example.com",
        },
      },
    }));
    const mv = new MaskedValue(
      {
        sentinel: "__zsmask__",
        masked: "***-**-6789",
        classification: "spi",
      },
      { collection: "users", row_pk: "usr_01", column: "ssn" },
    );
    const out = await mv.unmask(["ssn", "email"], {
      actor: { kind: "user", id: "actor_x" },
      reason: "support",
    });
    assert.ok(lastBulkArgs, "stub must have received the call");
    assert.equal(lastBulkArgs!.collection, "users");
    assert.equal(lastBulkArgs!.items.length, 1);
    assert.equal(lastBulkArgs!.items[0].rowPk, "usr_01");
    assert.deepEqual(lastBulkArgs!.items[0].columns, ["ssn", "email"]);
    assert.deepEqual(lastBulkArgs!.actor, { kind: "user", id: "actor_x" });
    assert.equal(lastBulkArgs!.reason, "support");
    assert.equal(out.ssn, "123-45-6789");
    assert.equal(out.email, "alice@example.com");
  });

  test("single-column overload still routes through unmaskField (no regression)", async () => {
    // Install both endpoints — the single-column path must NOT hit
    // bulkUnmaskFields, even though the stub above is wired.
    let unmaskFieldHits = 0;
    (globalThis as { env?: { db?: Record<string, unknown> } }).env = {
      db: {
        unmaskField: async (args: {
          collection: string;
          row_pk: string;
          column: string;
        }) => {
          unmaskFieldHits++;
          return { plaintext: `pt:${args.column}` };
        },
        bulkUnmaskFields: async () => {
          throw new Error("single-col path must NOT hit bulkUnmaskFields");
        },
      },
    };
    const mv = new MaskedValue(
      {
        sentinel: "__zsmask__",
        masked: "***",
        classification: "spi",
      },
      { collection: "users", row_pk: "usr_01", column: "ssn" },
    );
    const plaintext = await mv.unmask({ actor: { kind: "auto" } });
    assert.equal(plaintext, "pt:ssn");
    assert.equal(unmaskFieldHits, 1, "single-col call must reach unmaskField");
  });

  test("multi-column overload surfaces bulk_unmask_partial_unauthorized via the typed code", async () => {
    installStub(async () => {
      throw Object.assign(new Error("denied"), {
        code: "bulk_unmask_partial_unauthorized",
      });
    });
    const mv = new MaskedValue(
      {
        sentinel: "__zsmask__",
        masked: "***",
        classification: "spi",
      },
      { collection: "users", row_pk: "usr_01", column: "ssn" },
    );
    await assert.rejects(
      () => mv.unmask(["ssn"], { actor: { kind: "user" } }),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "bulk_unmask_partial_unauthorized",
    );
  });

  test("missing env.db.bulkUnmaskFields throws bulk_unmask_not_available", async () => {
    (globalThis as { env?: { db?: Record<string, unknown> } }).env = {
      db: {},
    };
    const mv = new MaskedValue(
      {
        sentinel: "__zsmask__",
        masked: "***",
        classification: "spi",
      },
      { collection: "users", row_pk: "usr_01", column: "ssn" },
    );
    await assert.rejects(
      () => mv.unmask(["ssn"], { actor: { kind: "auto" } }),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "bulk_unmask_not_available",
    );
  });

  test("missing row from native results yields an empty record", async () => {
    // Native returned results but for a different rowPk — defensive
    // handling, the SDK must not throw, it just returns `{}`.
    installStub(async () => ({
      results: {
        usr_other: { ssn: "should-not-leak" },
      },
    }));
    const mv = new MaskedValue(
      {
        sentinel: "__zsmask__",
        masked: "***",
        classification: "spi",
      },
      { collection: "users", row_pk: "usr_01", column: "ssn" },
    );
    const out = await mv.unmask(["ssn"], { actor: { kind: "auto" } });
    assert.deepEqual(out, {});
  });
});
