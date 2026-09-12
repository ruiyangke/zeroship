/**
 * `installSchema` — the framework-internal helper the synthetic SSR entry
 * and dev-bootstrap call to install a schema onto the native handle. This
 * test pins the install *mechanics*, independently of the surrounding
 * wiring:
 *
 * 1. The supplied `env` (the native handle) is unconditionally mutated with
 *    typed SDK Collection wrappers plus `transaction` / `live` extension
 *    methods. `Object.defineProperty` is the mechanism (`configurable:
 *    true`), so a second `installSchema` call with overlapping names
 *    re-installs without throwing.
 *
 * 2. A schema name colliding with the native v8_class method surface (e.g.
 *    `collection`, `transaction`, ...) throws, silently shadowing the
 *    native `env.db.collection` mint would be worse than a clear boot-time
 *    error.
 *
 * Every case needs a runtime schema descriptor. Collections come from the
 * descriptor alone, so an install with none has nothing to apply these
 * mechanics to and the assertions below would pass while testing nothing.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { installSchema } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";
import { descriptorFor } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";

/**
 * Install `schemas` the way the toolchain would: derive the descriptor from
 * the declaration, then hand both to the installer.
 */
function install(schemas: Record<string, unknown>, native: NativeDb) {
  return installSchema(schemas as never, native, {
    descriptor: descriptorFor(schemas),
  } as never);
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

  test("throws when a schema name collides with a native v8_class method", () => {
    const native = makeMockNative();
    for (const reserved of [
      "collection",
      "openSubscription",
      "migrations",
      "transaction",
      "live",
    ]) {
      assert.throws(
        () => install({ [reserved]: { name: t.string().required() } }, native),
        /collides with a native env.db method/,
        `reserved name "${reserved}" must throw`,
      );
    }
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
    // HMR storms in practice are serialised by the dev-bootstrap's
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
      options: { softDelete: false, versioning: false },
      indexes: [],
    };
    const reentrantDescriptor = {
      version: 2,
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
    installSchema({} as never, makeMockNative(), {
      descriptor: reentrantDescriptor,
    } as never);
    assert.ok(caught instanceof Error, "re-entrant call must throw");
    assert.equal((caught as { code?: string }).code, "INSTALL_IN_FLIGHT");
  });
});
