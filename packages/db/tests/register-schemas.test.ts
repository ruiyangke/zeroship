/**
 * `installSchema` — the host adapter helper that installs a descriptor onto
 * the native handle. This
 * test pins the install *mechanics*, independently of the surrounding
 * wiring:
 *
 * 1. The supplied `env` (the native handle) is unconditionally mutated with
 *    typed SDK Collection wrappers plus `transaction` / `live` extension
 *    methods. `Object.defineProperty` is the mechanism (`configurable:
 *    true`), so a second `installSchema` call with overlapping names
 *    re-installs without throwing.
 *
 * 2. A table name colliding with the native method surface stays available
 *    through name lookup without replacing that method.
 *
 * Every case needs a runtime schema descriptor. Collections come from the
 * descriptor alone, so an install with none has nothing to apply these
 * mechanics to and the assertions below would pass while testing nothing.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchema } from "../../../crates/zeroship-data-v8/js/testing.js";
import { t } from "../src/index.js";
import { descriptorFor } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";

/**
 * Install `schemas` the way the toolchain would: derive the descriptor from
 * the declaration, then hand both to the installer.
 */
function install(schemas: Record<string, unknown>, native: NativeDb) {
  return installSchema(native, descriptorFor(schemas) as never);
}

/**
 * Build a permissive mock native. The Collection wrappers SDK installs
 * onto it are the subject under test; we don't exercise CRUD here.
 */
function makeMockNative() {
  return {
    // Native `transaction(callback)` orchestrator stub.
    async transaction(cb: (raw: unknown) => unknown) {
      return cb(undefined);
    },
    collection(_name: string) {
      return {
        async findOne() { return null; },
        async find() { return []; },
        async insert(row: Record<string, unknown>) { return row; },
      };
    },
  } as unknown as NativeDb;
}

describe("installSchema", () => {
  test("plants typed Collection wrappers on the supplied env handle", () => {
    const native = makeMockNative();
    const { collections } = install(
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
    assert.equal((collections as Record<string, unknown>).users, handle.users);
    assert.equal((collections as Record<string, unknown>).todos, handle.todos);
  });

  test("plants the `transaction` and `live` extension methods on env", () => {
    const native = makeMockNative();
    install({ items: { name: t.string().required() } }, native);
    const handle = native as unknown as Record<string, unknown>;
    assert.equal(typeof handle.transaction, "function", "transaction is planted");
    assert.equal(typeof handle.live, "function", "live is planted");
  });

  test("re-entrant: a second call redefines the same name without throwing", () => {
    const native = makeMockNative();
    install({ users: { name: t.string().required() } }, native);
    const first = (native as unknown as { users: unknown }).users;
    // Second call with the same name — must not throw on the
    // `Object.defineProperty` because the descriptor is `configurable: true`.
    install({ users: { name: t.string().required(), email: t.string() } }, native);
    const second = (native as unknown as { users: unknown }).users;
    assert.notEqual(first, second, "second install replaced the wrapper");
  });

  test("replaces native direct aliases with the same SDK contract on first install", () => {
    const native = makeMockNative();
    const nativeAlias = native.collection("todos");
    Object.defineProperty(native, "todos", {
      value: nativeAlias,
      configurable: true,
      enumerable: true,
      writable: false,
    });

    install({ todos: { title: t.string().required() } }, native);
    const first = (native as unknown as { todos: unknown }).todos;
    assert.notEqual(first, nativeAlias, "host preparation must install the typed SDK facade");

    install({ todos: { title: t.string().required() } }, native);
    const second = (native as unknown as { todos: unknown }).todos;
    assert.notEqual(second, nativeAlias, "reinstall must preserve the SDK facade contract");
  });

  test("keeps methods callable and resolves colliding tables by name", async () => {
    for (const reserved of [
      "collection",
      "transaction",
      "live",
      "constructor",
    ]) {
      const native = makeMockNative();
      const nativeLookup = native.collection;
      const { collections } = install(
        { [reserved]: { name: t.string().required() } },
        native,
      );
      assert.ok((collections as Record<string, unknown>)[reserved]);
      assert.equal(native.collection, nativeLookup, `${reserved} must preserve collection(name)`);
      assert.equal(
        typeof (native as unknown as Record<string, unknown>)[reserved],
        "function",
        `${reserved} must remain callable`,
      );

      const result = await native.collection(reserved).find({});
      assert.deepEqual(result, []);
    }
  });

  test("treats former db method names as ordinary direct collections", () => {
    for (const name of ["migrations", "openSubscription"]) {
      const native = makeMockNative();
      const { collections } = install(
        { [name]: { title: t.string().required() } },
        native,
      );
      assert.equal(
        (native as unknown as Record<string, unknown>)[name],
        (collections as Record<string, unknown>)[name],
      );
    }
  });

  test("preserves a __proto__ collection through descriptor installation", () => {
    const native = makeMockNative();
    const declared = Object.create(null) as Record<string, unknown>;
    declared.__proto__ = { title: t.string().required() };
    const descriptorCollections = Object.create(null) as Record<string, unknown>;
    descriptorCollections.__proto__ = {
      fields: { id: { type: "string", required: true, primaryKey: true } },
      indexes: [],
    };

    const { collections } = installSchema(native, { collections: descriptorCollections } as never);

    assert.ok(Object.hasOwn(collections, "__proto__"));
    assert.equal(typeof native.collection("__proto__").find, "function");
    assert.equal(
      typeof (native as unknown as Record<string, unknown>).__proto__,
      "object",
      "the inherited object member remains lookup-only",
    );
  });

  test("beginTransaction is not a reserved env.db name", () => {
    // The native `beginTransaction` primitive was deleted entirely, so a
    // collection named `beginTransaction` no longer collides. (A creator
    // would be unwise to name a collection this, but the platform no
    // longer forbids it.)
    const native = makeMockNative();
    assert.doesNotThrow(
      () => install({ beginTransaction: { name: t.string().required() } }, native),
      "beginTransaction must not be a reserved name",
    );
    assert.ok(
      (native as unknown as Record<string, unknown>).beginTransaction,
      "and it installs as an ordinary collection",
    );
  });

  test("re-entrant install throws install_in_flight", () => {
    // The install path is synchronous, so the guard only fires on true
    // re-entry within one stack frame — e.g. a getter on the descriptor's
    // collection map that recursively calls back into `installSchema`.
    // Repeated host preparation is serialized by the runtime's
    // `schemaRegistered` latch; this guard exists for the case where that
    // latch is bypassed.
    //
    // The getter has to sit on the DESCRIPTOR rather than the declared
    // schema: the installer reads its collections from there, so that is
    // the only map it enumerates while an install is in flight.
    const innerNative = makeMockNative();
    let caught: unknown = null;
    const collection = {
      fields: { id: { type: "string", required: true, primaryKey: true }, name: { type: "string", required: true } },
      indexes: [],
    };
    const reentrantDescriptor = {
      collections: {
        get first() {
          try {
            install({ other: { name: t.string().required() } }, innerNative);
          } catch (e) {
            caught = e;
          }
          return collection;
        },
      },
    };
    installSchema(makeMockNative(), reentrantDescriptor as never);
    assert.ok(caught instanceof Error, "re-entrant call must throw");
    assert.equal((caught as { code?: string }).code, "INSTALL_IN_FLIGHT");
  });
});

test("installer bookkeeping does not occupy a creator collection name", () => {
  const native = makeMockNative();
  const { collections } = install({
    __zeroshipDbInstalledNames: { value: t.string().required() },
  }, native);
  const handle = native as unknown as Record<string, unknown>;
  assert.equal(handle.__zeroshipDbInstalledNames, collections.__zeroshipDbInstalledNames);
  assert.equal(typeof (handle.__zeroshipDbInstalledNames as { find: unknown }).find, "function");
});

test("reinstallation ignores forged bookkeeping and removes only its stale collections", () => {
  const native = makeMockNative();
  const handle = native as unknown as Record<string, unknown>;
  install({ before: { value: t.string().required() } }, native);
  handle.applicationState = "keep";
  Object.defineProperty(handle, "__zeroshipDbInstalledNames", {
    value: ["applicationState"], configurable: true, writable: true,
  });
  install({ after: { value: t.string().required() } }, native);
  assert.equal(handle.applicationState, "keep");
  assert.equal(Object.hasOwn(handle, "before"), false);
  assert.equal(typeof (handle.after as { find: unknown }).find, "function");
});
