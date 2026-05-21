/**
 * `_installSchema` — the framework-internal helper that the synthetic
 * SSR entry and dev-bootstrap call to register schemas declared via
 * `export default { schema }`. This test pins the two behaviours that
 * matter independently of the surrounding wiring:
 *
 * 1. `installOnEnvDb: true` mutates the supplied `native` handle so
 *    `native.<collection>` resolves to the typed SDK Collection wrapper.
 *    `Object.defineProperty` is the mechanism (`configurable: true`),
 *    so a second `_installSchema` call with overlapping names
 *    re-installs without throwing.
 *
 * 2. A schema name colliding with the native v8_class method surface
 *    (e.g. `collection`, `beginTransaction`, ...) throws — silently
 *    shadowing the native `env.db.collection` mint would be worse
 *    than a clear boot-time error.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { _installSchema } from "../src/db.js";
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

describe("_installSchema", () => {
  test("installOnEnvDb mutates the native handle with typed Collection wrappers", () => {
    const native = makeMockNative();
    _installSchema(
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

  test("without installOnEnvDb (test path) does NOT mutate env.db", () => {
    const native = makeMockNative();
    _installSchema(
      { posts: { title: t.string().required() } },
      { native /* installOnEnvDb omitted */ },
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(handle.posts, undefined, "no install when option omitted");
  });

  test("re-entrant: a second call redefines the same name without throwing", () => {
    const native = makeMockNative();
    _installSchema(
      { users: { name: t.string().required() } },
      { native, installOnEnvDb: true },
    );
    const first = (native as unknown as { users: unknown }).users;
    // Second call with the same name — must not throw on the
    // `Object.defineProperty` because the descriptor is `configurable: true`.
    _installSchema(
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
        () => _installSchema(
          { [reserved]: { name: t.string().required() } },
          { native, installOnEnvDb: true },
        ),
        /collides with a native env.db method/,
        `reserved name "${reserved}" must throw`,
      );
    }
  });

  test("__zeroshipPlatformReady reflects a synchronous install failure", async () => {
    // R3 IMPORTANT-1 regression. A reserved-name collision throws
    // synchronously from `_installSchema` AFTER (in the prior layout)
    // the platform-ready chain had already been published. The auto-tx
    // dispatcher would then await that stale chain — which resolved on
    // registerModel success — and dispatch against an `env.db` whose
    // collections were never bound. The fix publishes platform-ready
    // only on success, and stamps a rejected promise on synchronous
    // throw so awaiters see the failure.
    const g = globalThis as { __zeroshipPlatformReady?: Promise<unknown> };
    delete g.__zeroshipPlatformReady;
    const native = makeMockNative();
    assert.throws(
      () => _installSchema(
        { collection: { name: t.string().required() } },
        { native, installOnEnvDb: true },
      ),
      /collides with a native env.db method/,
    );
    // The published handle must exist and must reject — the auto-tx
    // dispatcher's `try { await ready } catch {}` then surfaces the
    // failure instead of awaiting a (stale) resolved promise.
    const ready = g.__zeroshipPlatformReady;
    assert.ok(ready instanceof Promise, "platform-ready must be published on failure");
    let caught: unknown = null;
    try { await ready; } catch (e) { caught = e; }
    assert.ok(caught instanceof Error, "published handle must reject with the install error");
    assert.match((caught as Error).message, /collides with a native env\.db method/);
    delete g.__zeroshipPlatformReady;
  });

  test("__zeroshipPlatformReady is NOT published until install completes", async () => {
    // The publish must happen AFTER env.db own-properties are bound,
    // not before the install block. Verify that on a successful
    // install, `globalThis.__zeroshipPlatformReady` is set AND
    // `env.db.<name>` is bound at the same observation point.
    const g = globalThis as { __zeroshipPlatformReady?: Promise<unknown> };
    delete g.__zeroshipPlatformReady;
    const native = makeMockNative();
    _installSchema(
      { items: { name: t.string().required() } },
      { native, installOnEnvDb: true },
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.ok(handle.items, "env.db.items must be bound by the time the publish lands");
    assert.ok(g.__zeroshipPlatformReady instanceof Promise, "platform-ready must be published on success");
    await g.__zeroshipPlatformReady;
    delete g.__zeroshipPlatformReady;
  });

  test("re-entrant install throws install_in_flight", () => {
    // R3 IMPORTANT-3 regression. The install path is synchronous, so
    // the guard only fires on true re-entry within one stack frame —
    // e.g. a getter on the schema map that recursively calls back
    // into `_installSchema`. HMR storms in practice are serialised by
    // the dev-bootstrap's `schemaRegistered` latch; this guard exists
    // for the case where that latch is bypassed.
    const g = globalThis as { __zeroshipPlatformReady?: Promise<unknown> };
    delete g.__zeroshipPlatformReady;
    const innerNative = makeMockNative();
    let caught: unknown = null;
    // A schema map whose first key is read via a getter that synchronously
    // re-enters `_installSchema`. The outer install begins, reads
    // `Object.entries(schemas)` (which fires the getter), and the
    // inner call must throw `install_in_flight`.
    const reentrantSchema = {
      get first(): { name: ReturnType<typeof t.string> } {
        try {
          _installSchema(
            { other: { name: t.string().required() } },
            { native: innerNative, installOnEnvDb: true },
          );
        } catch (e) {
          caught = e;
        }
        return { name: t.string().required() };
      },
    } as { first: { name: ReturnType<typeof t.string> } };
    _installSchema(
      reentrantSchema,
      { native: makeMockNative(), installOnEnvDb: true },
    );
    assert.ok(caught instanceof Error, "re-entrant call must throw");
    assert.equal((caught as { code?: string }).code, "install_in_flight");
    delete g.__zeroshipPlatformReady;
  });
});
