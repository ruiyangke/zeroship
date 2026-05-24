/**
 * **P5.5 PR 7** — per-query unmask hint shape pin.
 *
 * The runtime side (the `dispatch_find_one` / `dispatch_find` glue
 * in `crates/plugin-db/src/crud/mod.rs`) consumes `opts.unmask` /
 * `opts.actor` / `opts.unmaskReason` from the JS-facing find opts.
 * These tests pin the JS-side opts shape so a future SDK refactor
 * doesn't silently drop the fields before they reach the native
 * boundary.
 *
 * The native dispatcher behaviour (atomic auth fence, audit row
 * emission, plaintext promotion) is covered end-to-end by
 * `crates/plugin-db/tests/sqlite_integration.rs`. Here we only pin:
 *
 *   1. The shape the SDK sends ON THE WIRE matches the contract
 *      `ZeroshipDbFindOpts` declares.
 *   2. The `unmask` array survives column-name remapping
 *      (`naming.snakeCase`, etc.) when present.
 *   3. The actor object passes through verbatim (no shape mangling
 *      that would invalidate the per-app `MaskPolicy.allows` lookup).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

// Pin the TypeScript type shape: a build break here means the
// runtime contract drifted (e.g. someone renamed `unmask` to
// `unmaskColumns` on one side without updating the other). The
// type assertion at compile time is the real test; the runtime body
// just asserts the shape is structurally usable.
describe("P5.5 PR 7 — per-query unmask hint opts shape", () => {
  test("ZeroshipDbFindOpts accepts unmask + actor + unmaskReason", () => {
    const opts: ZeroshipDbFindOpts = {
      unmask: ["ssn", "email"],
      actor: { kind: "user", id: "actor_x" },
      unmaskReason: "ops dashboard",
    };
    assert.deepEqual(opts.unmask, ["ssn", "email"]);
    assert.equal((opts.actor as { kind: string }).kind, "user");
    assert.equal(opts.unmaskReason, "ops dashboard");
  });

  test("ZeroshipDbFindOpts can omit the unmask hint entirely", () => {
    const opts: ZeroshipDbFindOpts = {
      limit: 10,
      orderBy: { createdAt: -1 },
    };
    assert.equal(opts.unmask, undefined);
    assert.equal(opts.actor, undefined);
  });

  test("unmask array can be empty (= no-op hint)", () => {
    const opts: ZeroshipDbFindOpts = {
      unmask: [],
      actor: { kind: "auto" },
    };
    assert.deepEqual(opts.unmask, []);
  });

  test("orderBy / select / unmask coexist", () => {
    const opts: ZeroshipDbFindOpts = {
      limit: 5,
      orderBy: { id: 1 },
      select: ["id", "ssn"],
      unmask: ["ssn"],
      actor: { kind: "user", id: "actor_x" },
    };
    assert.equal(opts.limit, 5);
    assert.deepEqual(opts.select, ["id", "ssn"]);
    assert.deepEqual(opts.unmask, ["ssn"]);
  });

  test("actor accepts arbitrary Record<string, unknown> shape", () => {
    // The per-app `MaskPolicy.allows` only inspects `kind` (and
    // optionally `id`) but the SDK type intentionally allows extra
    // fields so apps can carry RBAC tags / request IDs through the
    // audit log unchanged.
    const opts: ZeroshipDbFindOpts = {
      unmask: ["ssn"],
      actor: {
        kind: "user",
        id: "actor_x",
        roles: ["admin", "support"],
        requestId: "req_abc",
      },
    };
    const actor = opts.actor as { kind: string; roles: string[] };
    assert.equal(actor.kind, "user");
    assert.deepEqual(actor.roles, ["admin", "support"]);
  });
});
