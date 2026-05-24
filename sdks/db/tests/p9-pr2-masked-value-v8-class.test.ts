/**
 * **P9 PR 2** — `MaskedValue` promoted to a native v8_class +
 * Rust-side rehydration.
 *
 * Two SDK-observable consequences are pinned here:
 *
 *  1. The JS-side `__zsmask__` rehydration loop is GONE. Masked
 *     columns are minted as native `MaskedValue` v8_class instances
 *     Rust-side at `JSON.parse` time (`ResolveValue::JsonWithRehydration`
 *     → `masked_value::rehydrate_masked_values`), so `mapResultDoc`
 *     only renames keys and passes the native instances through
 *     unchanged.
 *
 *  2. `Db.unmaskField` / `Db.bulkUnmaskFields` were removed. The bulk
 *     unmask round-trip is now collection-scoped: `Collection.bulkUnmask`
 *     (collection name inherited from the receiver). The SDK's
 *     `Collection.bulkUnmask` routes to that native method, maps the
 *     requested columns through `_toColumn`, and maps the per-row
 *     plaintext map back through `_toField`.
 *
 * The RUNTIME behaviour of the native `MaskedValue` (brand check,
 * `toString` / `toJSON` → masked string, `unmask` success / failure,
 * multi-column unmask, `canUnmask` probe) is exercised by the Rust
 * unit tests in `crates/plugin-db/src/v8_classes/masked_value.rs` and
 * the SQLite integration target — they require the V8 + plugin-db
 * runtime, which this Node test harness does not host. The type-level
 * checks below pin the SDK's ambient `declare class MaskedValue` shape.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchemaForTest } from "./_install-helper.js";
import { t, schema as schemaWrap } from "@zeroship/db";
import { mapResultDoc } from "../src/utils.js";
import type { Row, MaskedValue } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

// ---------------------------------------------------------------------------
// 1. mapResultDoc no longer rehydrates — native MaskedValue passes through
// ---------------------------------------------------------------------------

describe("P9 PR 2 — mapResultDoc passes native MaskedValue instances through", () => {
  const identity = (s: string) => s;

  test("a native-MaskedValue-shaped value is NOT reconstructed (passes by reference)", () => {
    // Simulate a native MaskedValue instance: an opaque object the SDK
    // must NOT touch. Pre-P9 the SDK recognised `sentinel: "__zsmask__"`
    // and rebuilt it; now the runtime has already minted the instance,
    // so `mapResultDoc` must return the SAME object reference.
    const nativeMasked = { masked: "***-**-6789", classification: "spi" };
    const doc: AnyRec = { id: "usr_1", ssn: nativeMasked };
    const out = mapResultDoc(doc, identity);
    assert.equal(out.ssn, nativeMasked, "MaskedValue instance must pass by reference");
    assert.equal(out.id, "usr_1");
  });

  test("a raw __zsmask__ sentinel object is NOT rehydrated by the SDK anymore", () => {
    // Even if a sentinel-shaped object somehow reached the SDK, the JS
    // rehydration arm is deleted — it must pass through verbatim (the
    // runtime is responsible for replacing it before the SDK sees it).
    const sentinel = {
      sentinel: "__zsmask__",
      masked: "***",
      classification: "pii",
      _meta: { collection: "users", row_pk: "usr_1", column: "ssn" },
    };
    const out = mapResultDoc({ ssn: sentinel }, identity);
    assert.equal(out.ssn, sentinel, "sentinel must pass through untouched");
  });

  test("key renaming still applies (general mapping is intact)", () => {
    const out = mapResultDoc({ user_name: "alice" }, (c) => (c === "user_name" ? "userName" : c));
    assert.equal(out.userName, "alice");
    assert.equal(out.user_name, undefined);
  });
});

// ---------------------------------------------------------------------------
// 2. Collection.bulkUnmask routes to the native Collection.bulkUnmask
// ---------------------------------------------------------------------------

/** Native double exposing a recording `Collection.bulkUnmask`. The
 *  removed `Db.bulkUnmaskFields` is deliberately absent so a regression
 *  that routes through it would throw `is not a function`. */
function makeNativeWithBulkUnmask(
  handler: (
    items: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>,
    opts: { actor?: unknown; reason?: string },
  ) => Promise<{ results: Record<string, Record<string, unknown>> }>,
) {
  const captured: {
    items?: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>;
    opts?: { actor?: unknown; reason?: string };
  } = {};
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async bulkUnmask(
          items: ReadonlyArray<{ rowPk: string; columns: readonly string[] }>,
          opts: { actor?: unknown; reason?: string },
        ) {
          captured.items = items;
          captured.opts = opts;
          return handler(items, opts);
        },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, captured };
}

/** Native double whose Collection lacks `bulkUnmask` (legacy runtime). */
function makeNativeMissingBulkUnmask() {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {};
    },
  };
  return native as unknown as ZeroshipDb;
}

describe("P9 PR 2 — Collection.bulkUnmask → native Collection.bulkUnmask", () => {
  function makeDb(native: ZeroshipDb) {
    return installSchemaForTest(
      {
        users: schemaWrap({
          ssn: t.encrypted({ wraps: t.string() }).mask({ kind: "last4", classification: "spi" }),
          email: t.string().mask({ kind: "email" }),
        }),
      },
      { native },
    );
  }

  test("routes through native Collection.bulkUnmask with the right wire items", async () => {
    const { native, captured } = makeNativeWithBulkUnmask(async () => ({
      results: { usr_01: { ssn: "123-45-6789", email: "alice@example.com" } },
    }));
    const db = makeDb(native);
    const result = await db.users.bulkUnmask(
      [{ id: "usr_01", columns: ["ssn", "email"] }],
      { actor: { kind: "user", id: "actor_x" }, reason: "ops" },
    );
    assert.equal(result.error, null);
    assert.ok(captured.items, "native bulkUnmask must have been called");
    assert.equal(captured.items!.length, 1);
    assert.equal(captured.items![0].rowPk, "usr_01");
    assert.deepEqual(captured.items![0].columns, ["ssn", "email"]);
    assert.deepEqual(captured.opts!.actor, { kind: "user", id: "actor_x" });
    assert.equal(captured.opts!.reason, "ops");
  });

  test("maps the per-row plaintext map back into field-name space", async () => {
    const { native } = makeNativeWithBulkUnmask(async () => ({
      results: { usr_01: { ssn: "123-45-6789" } },
    }));
    const db = makeDb(native);
    const result = await db.users.bulkUnmask(
      [{ id: "usr_01", columns: ["ssn"] }],
      { actor: { kind: "auto" } },
    );
    assert.equal(result.error, null);
    const map = result.data!;
    assert.ok(map.has("usr_01"));
    assert.equal(map.get("usr_01")!.ssn, "123-45-6789");
  });

  test("surfaces bulk_unmask_partial_unauthorized via the Result envelope", async () => {
    const { native } = makeNativeWithBulkUnmask(async () => {
      throw Object.assign(new Error("denied"), {
        code: "bulk_unmask_partial_unauthorized",
      });
    });
    const db = makeDb(native);
    const result = await db.users.bulkUnmask(
      [{ id: "usr_01", columns: ["ssn"] }],
      { actor: { kind: "user" } },
    );
    assert.ok(result.error, "expected a Result.error");
    assert.equal((result.error as { code?: string }).code, "bulk_unmask_partial_unauthorized");
  });

  test("missing native Collection.bulkUnmask surfaces bulk_unmask_not_available", async () => {
    const native = makeNativeMissingBulkUnmask();
    const db = installSchemaForTest(
      { users: schemaWrap({ ssn: t.encrypted({ wraps: t.string() }) }) },
      { native },
    );
    const result = await db.users.bulkUnmask(
      [{ id: "usr_01", columns: ["ssn"] }],
      { actor: { kind: "auto" } },
    );
    assert.ok(result.error);
    assert.equal((result.error as { code?: string }).code, "bulk_unmask_not_available");
  });

  test("a native row missing from results yields an empty record for that row", async () => {
    const { native } = makeNativeWithBulkUnmask(async () => ({
      // Native returned a different rowPk — defensive: SDK must not throw.
      results: { usr_other: { ssn: "should-not-leak" } },
    }));
    const db = makeDb(native);
    const result = await db.users.bulkUnmask(
      [{ id: "usr_01", columns: ["ssn"] }],
      { actor: { kind: "auto" } },
    );
    assert.equal(result.error, null);
    // The requested row isn't in the returned map.
    assert.equal(result.data!.has("usr_01"), false);
  });
});

// ---------------------------------------------------------------------------
// 3. Type-level: Row<S> wraps masked columns in MaskedValue<T>; the
//    ambient declare class shape is honoured.
// ---------------------------------------------------------------------------

describe("P9 PR 2 — MaskedValue declare-class type surface (compile-time)", () => {
  // Compile-time assertion helper: a no-op at runtime; the win is `tsc`
  // rejecting a mismatched assignment.
  function assertType<T>(_v: T): void {
    /* noop */
  }

  test("Row<S>['ssn'] is MaskedValue<string>", () => {
    const fields = {
      ssn: t.encrypted({ wraps: t.string() }).mask({ kind: "last4", classification: "spi" }).required(),
      name: t.string().required(),
    };
    type R = Row<typeof fields>;
    // `as unknown as` casts because MaskedValue has no runtime constructor.
    const row: R = {
      id: 1,
      name: "Alice",
      ssn: "***-**-6789" as unknown as MaskedValue<string>,
      createdAt: 0,
      updatedAt: 0,
    };
    assertType<MaskedValue<string>>(row.ssn);
    assertType<string>(row.name);
    assert.equal(row.name, "Alice");
  });

  test("the declare class exposes the documented getters + method return types", () => {
    // Pure type-level: the assertions live inside a function that is
    // NEVER invoked (the methods are called on a `null` placeholder, so
    // executing them would throw). `tsc` still typechecks the body — if
    // a getter or method were dropped from the declare class, or its
    // signature changed, this file fails to compile.
    // eslint-disable-next-line @typescript-eslint/no-unused-vars
    const _typecheck = (mv: MaskedValue<string>) => {
      assertType<string>(mv.masked);
      assertType<string>(mv.classification);
      assertType<Readonly<{ collection: string; row_pk: string; column: string }>>(mv._meta);
      assertType<Promise<string>>(mv.unmask());
      assertType<Promise<string>>(mv.unmask({ actor: { kind: "user" }, reason: "x" }));
      assertType<Promise<boolean>>(mv.canUnmask());
      assertType<Promise<boolean>>(mv.canUnmask({ actor: { kind: "user" } }));
      assertType<string>(mv.toString());
      assertType<string>(mv.toJSON());
      // The multi-column overload returns a per-column record.
      assertType<Promise<Record<string, string>>>(
        mv.unmask(["ssn", "email"], { actor: { kind: "user" } }),
      );
    };
    // No runtime work — the type assertions above are the gate.
    assert.ok(true);
  });
});
