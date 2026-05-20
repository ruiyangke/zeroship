/**
 * `__registerSchemas` — the internal helper behind both `createDb()` and
 * the dev-bootstrap default-export discovery path. This test pins the
 * two behaviours that distinguish it from a bare `createDb`:
 *
 * 1. `installOnEnvDb: true` mutates the supplied `native` handle so
 *    `native.<collection>` resolves to the typed SDK Collection wrapper.
 *    `Object.defineProperty` is the mechanism (`configurable: true`),
 *    so a second `__registerSchemas` call with overlapping names
 *    re-installs without throwing.
 *
 * 2. A schema name colliding with the native v8_class method surface
 *    (e.g. `collection`, `beginTransaction`, ...) throws — silently
 *    shadowing the native `env.db.collection` mint would be worse
 *    than a clear boot-time error.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { __registerSchemas } from "../src/db.js";
import { t } from "../src/types.js";

/**
 * Build a permissive mock `native` whose `registerModel` resolves
 * immediately. The Collection wrappers SDK installs onto it are the
 * subject under test — we don't exercise CRUD here.
 */
function makeMockNative() {
  return {
    async registerModel(_name: string, _schema: unknown, _indexes?: unknown): Promise<void> {
      // no-op
    },
    async beginTransaction() {
      return { async commit() { /* */ }, async rollback() { /* */ } };
    },
    collection(_name: string) {
      return {
        async findOne() { return null; },
        async find() { return []; },
        async insert(row: Record<string, unknown>) { return row; },
      };
    },
  } as unknown as ZeroshipDb;
}

describe("__registerSchemas", () => {
  test("installOnEnvDb mutates the native handle with typed Collection wrappers", () => {
    const native = makeMockNative();
    __registerSchemas(
      {
        users: { name: t.string().required() },
        todos: { title: t.string().required() },
      },
      { native, installOnEnvDb: true },
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.ok(handle.users, "users wrapper installed");
    assert.ok(handle.todos, "todos wrapper installed");
    // The wrapper exposes the SDK's Collection methods (find, insert, ...)
    // — distinct from the bare native CRUD surface.
    assert.equal(typeof (handle.users as { insert?: unknown }).insert, "function");
    assert.equal(typeof (handle.todos as { update?: unknown }).update, "function");
  });

  test("default createDb path does NOT mutate env.db", () => {
    const native = makeMockNative();
    __registerSchemas(
      { posts: { title: t.string().required() } },
      { native /* installOnEnvDb omitted */ },
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(handle.posts, undefined, "no install when option omitted");
  });

  test("re-entrant: a second call redefines the same name without throwing", () => {
    const native = makeMockNative();
    __registerSchemas(
      { users: { name: t.string().required() } },
      { native, installOnEnvDb: true },
    );
    const first = (native as unknown as { users: unknown }).users;
    // Second call with the same name — must not throw on the
    // `Object.defineProperty` because the descriptor is `configurable: true`.
    __registerSchemas(
      { users: { name: t.string().required(), email: t.string() } },
      { native, installOnEnvDb: true },
    );
    const second = (native as unknown as { users: unknown }).users;
    assert.notEqual(first, second, "second install replaced the wrapper");
  });

  test("throws when a schema name collides with a native v8_class method", () => {
    const native = makeMockNative();
    for (const reserved of [
      "collection",
      "beginTransaction",
      "registerModel",
      "openSubscription",
      "startReplicationConsumer",
      "migrations",
      "replication",
    ]) {
      assert.throws(
        () => __registerSchemas(
          { [reserved]: { name: t.string().required() } },
          { native, installOnEnvDb: true },
        ),
        /collides with a native env.db method/,
        `reserved name "${reserved}" must throw`,
      );
    }
  });
});
