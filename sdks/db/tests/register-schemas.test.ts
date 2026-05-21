/**
 * `installSchema` — the framework-internal helper that the synthetic
 * SSR entry and dev-bootstrap call to register schemas declared via
 * `export default { schema }`. This test pins the behaviours that
 * matter independently of the surrounding wiring:
 *
 * 1. The supplied `env` (the native handle) is unconditionally
 *    mutated with typed SDK Collection wrappers plus `transaction` /
 *    `live` extension methods. `Object.defineProperty` is the
 *    mechanism (`configurable: true`), so a second `installSchema`
 *    call with overlapping names re-installs without throwing.
 *
 * 2. A schema name colliding with the native v8_class method surface
 *    (e.g. `collection`, `beginTransaction`, ...) throws — silently
 *    shadowing the native `env.db.collection` mint would be worse
 *    than a clear boot-time error.
 *
 * 3. The returned `ready` promise resolves once the chained
 *    registerModel DDL has settled. A synchronous install failure
 *    (reserved-name collision) does NOT reject `ready` directly —
 *    the throw propagates to the caller — but the module-local
 *    prev-chain is updated so subsequent installs serialise behind
 *    it. The auto-tx dispatcher reads its `ready` handle from the
 *    install RETURN VALUE, so it only awaits a successful install's
 *    chain; failures are surfaced by the synchronous throw.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchema } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";

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

describe("installSchema", () => {
  test("plants typed Collection wrappers on the supplied env handle", () => {
    const native = makeMockNative();
    const { collections } = installSchema(
      {
        users: { name: t.string().required() },
        todos: { title: t.string().required() },
      },
      native,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.ok(handle.users, "users wrapper installed on env");
    assert.ok(handle.todos, "todos wrapper installed on env");
    // The wrapper exposes the SDK's Collection methods (find, insert, ...)
    // — distinct from the bare native CRUD surface.
    assert.equal(typeof (handle.users as { insert?: unknown }).insert, "function");
    assert.equal(typeof (handle.todos as { update?: unknown }).update, "function");
    // The returned `collections` is the same identity as what got
    // planted on env (single source of truth).
    assert.equal(collections.users, handle.users);
    assert.equal(collections.todos, handle.todos);
  });

  test("plants the `transaction` and `live` extension methods on env", () => {
    const native = makeMockNative();
    installSchema(
      { items: { name: t.string().required() } },
      native,
    );
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(typeof handle.transaction, "function", "transaction is planted");
    assert.equal(typeof handle.live, "function", "live is planted");
  });

  test("re-entrant: a second call redefines the same name without throwing", () => {
    const native = makeMockNative();
    installSchema(
      { users: { name: t.string().required() } },
      native,
    );
    const first = (native as unknown as { users: unknown }).users;
    // Second call with the same name — must not throw on the
    // `Object.defineProperty` because the descriptor is `configurable: true`.
    installSchema(
      { users: { name: t.string().required(), email: t.string() } },
      native,
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
      "transaction",
      "live",
    ]) {
      assert.throws(
        () => installSchema(
          { [reserved]: { name: t.string().required() } },
          native,
        ),
        /collides with a native env.db method/,
        `reserved name "${reserved}" must throw`,
      );
    }
  });

  test("returned `ready` resolves once registerModel has settled", async () => {
    const native = makeMockNative();
    const { ready } = installSchema(
      { items: { name: t.string().required() } },
      native,
    );
    assert.ok(ready instanceof Promise, "ready must be a Promise");
    // Resolves cleanly — the mock's registerModel is a no-op.
    await ready;
  });

  test("returned `ready` propagates DDL failures", async () => {
    // A native whose `registerModel` rejects models the case of a
    // schema-build failure (bad DDL, advisory-lock fight, ...). The
    // promise the SDK pins on the Collection rejects on first CRUD,
    // and the returned `ready` carries the same rejection so the
    // bootstrap can surface it during module evaluation.
    const native = {
      async registerModel(_name: string): Promise<void> {
        throw Object.assign(new Error("DDL bombed"), { code: "ddl_failed" });
      },
      async beginTransaction() {
        return { async commit() {}, async rollback() {} };
      },
      collection(_name: string) { return { async find() { return []; } }; },
    } as unknown as ZeroshipDb;
    const { ready } = installSchema(
      { items: { name: t.string().required() } },
      native,
    );
    let caught: unknown = null;
    try { await ready; } catch (e) { caught = e; }
    assert.ok(caught instanceof Error, "ready must reject on DDL failure");
    assert.equal((caught as Error).message, "DDL bombed");
  });

  test("subsequent install after a sync failure still serialises behind the prior chain", async () => {
    // A reserved-name collision throws synchronously from `installSchema`.
    // The module-local prev-chain is updated to a rejected promise so
    // the next install's `prev.catch(() => undefined).then(() => chain)`
    // observes the rejection but doesn't propagate it — the new
    // install's `ready` resolves on its own success.
    const native = makeMockNative();
    assert.throws(
      () => installSchema(
        { collection: { name: t.string().required() } },
        native,
      ),
      /collides with a native env.db method/,
    );
    // The next install on a fresh env handle must resolve cleanly —
    // the prior install's rejection is swallowed in the chain.
    const fresh = makeMockNative();
    const { ready } = installSchema(
      { items: { name: t.string().required() } },
      fresh,
    );
    await ready;
  });

  test("re-entrant install throws install_in_flight", () => {
    // The install path is synchronous, so the guard only fires on
    // true re-entry within one stack frame — e.g. a getter on the
    // schema map that recursively calls back into `installSchema`.
    // HMR storms in practice are serialised by the dev-bootstrap's
    // `schemaRegistered` latch; this guard exists for the case where
    // that latch is bypassed.
    const innerNative = makeMockNative();
    let caught: unknown = null;
    // A schema map whose first key is read via a getter that
    // synchronously re-enters `installSchema`. The outer install
    // begins, reads `Object.entries(schemas)` (which fires the
    // getter), and the inner call must throw `install_in_flight`.
    const reentrantSchema = {
      get first(): { name: ReturnType<typeof t.string> } {
        try {
          installSchema(
            { other: { name: t.string().required() } },
            innerNative,
          );
        } catch (e) {
          caught = e;
        }
        return { name: t.string().required() };
      },
    } as { first: { name: ReturnType<typeof t.string> } };
    installSchema(
      reentrantSchema,
      makeMockNative(),
    );
    assert.ok(caught instanceof Error, "re-entrant call must throw");
    assert.equal((caught as { code?: string }).code, "install_in_flight");
  });
});
