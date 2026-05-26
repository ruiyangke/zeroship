/**
 * Smoke tests for `@zeroship/bootstrap/install-schema`.
 *
 * The behavioural coverage of `installSchema`, `validateRefTargets`,
 * `normalizeSchema`, `expandUnionToFlatColumns`, `model`, and
 * `topoSortByRefs` lives in `sdks/db/tests/` (the existing test files
 * call into these helpers via the `_install-helper.ts` adapter and the
 * `@zeroship/bootstrap/install-schema` subpath). Those tests are the
 * source of truth for the moved code paths — Stage 7's hard rule was
 * "maintain test coverage", and the tests follow the helpers across
 * the package boundary.
 *
 * This file just sanity-checks the bootstrap package's public-to-
 * framework API surface so a fresh `pnpm -F @zeroship/bootstrap test`
 * has a non-zero count and a place to anchor future bootstrap-only
 * tests (dispatcher idempotency, normalizeUserModule edge cases, etc.).
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  installSchema,
  resolveDbPlatform,
  validateRefTargets,
  normalizeSchema,
  expandUnionToFlatColumns,
  model,
} from "../src/install-schema.js";
import { normalizeUserModule } from "../src/normalize.js";
import { t } from "@zeroship/db";

describe("@zeroship/bootstrap public surface", () => {
  test("installSchema is a function", () => {
    assert.equal(typeof installSchema, "function");
  });

  test("validateRefTargets is a function", () => {
    assert.equal(typeof validateRefTargets, "function");
  });

  test("normalizeSchema is a function", () => {
    assert.equal(typeof normalizeSchema, "function");
  });

  test("expandUnionToFlatColumns is a function", () => {
    assert.equal(typeof expandUnionToFlatColumns, "function");
  });

  test("model is a function", () => {
    assert.equal(typeof model, "function");
  });

  test("normalizeUserModule is a function", () => {
    assert.equal(typeof normalizeUserModule, "function");
  });
});

describe("normalizeSchema — minimal smoke", () => {
  test("turns a record of t.* builders into a NormalizedSchema", () => {
    const out = normalizeSchema({
      title: t.string().required(),
      done: t.boolean().default(false),
    });
    assert.equal(out.title.type, "string");
    assert.equal(out.title.required, true);
    assert.equal(out.done.type, "boolean");
    assert.equal(out.done.default, false);
  });
});

describe("normalizeSchema — P7 typed-id prefix (id: t.id(prefix))", () => {
  test("retains an id:t.id(prefix) field with its idPrefix", () => {
    const out = normalizeSchema({
      id: t.id("blog"),
      title: t.string(),
    });
    // The id prefix declaration survives normalization so registerModel
    // (and the Rust schema cache) can read the declared prefix.
    assert.equal(out.id.type, "id");
    assert.equal(out.id.idPrefix, "blog");
    assert.equal(out.title.type, "string");
  });

  test("rejects id declared with a non-id type", () => {
    assert.throws(
      () => normalizeSchema({ id: t.string() }),
      (err: unknown) => {
        assert.equal(
          (err as { code?: string }).code,
          "RESERVED_SYSTEM_FIELD_NAME",
        );
        return true;
      },
    );
  });

  test("rejects another reserved system field name (version)", () => {
    assert.throws(
      () => normalizeSchema({ version: t.number() }),
      (err: unknown) => {
        assert.equal(
          (err as { code?: string }).code,
          "RESERVED_SYSTEM_FIELD_NAME",
        );
        return true;
      },
    );
  });
});

describe("validateRefTargets — minimal smoke", () => {
  test("throws on a missing target collection", () => {
    try {
      validateRefTargets({
        posts: {
          title: t.string().required(),
          // simulates an `as any` escape past the TS check.
          authorId: t.ref("ghost"),
        },
      });
      assert.fail("expected throw");
    } catch (e) {
      const err = e as Error & { code?: string; target?: string };
      assert.equal(err.code, "REF_TARGET_NOT_FOUND");
      assert.equal(err.target, "ghost");
    }
  });

  test("accepts a ref to a declared collection", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string().required() },
        posts: { title: t.string().required(), authorId: t.ref("users") },
      });
    });
  });
});

describe("normalizeUserModule — minimal smoke", () => {
  test("merges default.rpc with named exports (named wins)", () => {
    const mod = {
      default: { rpc: { dup: () => "fromDefault", onlyDef: () => "d" } },
      dup: () => "named",
      named: () => "named-ok",
    };
    const out = normalizeUserModule(mod);
    assert.equal(typeof out.rpc.dup, "function");
    assert.equal((out.rpc.dup as () => string)(), "named");
    assert.equal((out.rpc.onlyDef as () => string)(), "d");
    assert.equal((out.rpc.named as () => string)(), "named-ok");
  });

  test("picks default.fetch when present, falls back to top-level fetch", () => {
    const fetchFn = () => new Response("ok");
    const mod = { default: { fetch: fetchFn } };
    const out = normalizeUserModule(mod);
    assert.equal(out.fetch, fetchFn);

    const mod2 = { fetch: fetchFn };
    const out2 = normalizeUserModule(mod2);
    assert.equal(out2.fetch, fetchFn);
  });

  test("surfaces schema from default.schema", () => {
    const mod = { default: { schema: { todos: {} } } };
    const out = normalizeUserModule(mod);
    assert.deepEqual(out.schema, { todos: {} });
  });
});

// ---------------------------------------------------------------------------
// P9 PR 4 — `__platform` capability handle routing (§8)
// ---------------------------------------------------------------------------
//
// `registerModel` / `setMaskPolicy` moved off `env.db` to the
// `__platform` handle the runtime stashes under a V8 private symbol and
// exposes via `globalThis.__zsDbPlatform(db)`. These tests pin the
// bootstrap-side wiring: `resolveDbPlatform` finds the handle, and
// `installSchema` routes registration THROUGH it (not through `env.db`)
// when present, while still falling back to a `registerModel` on the
// `env` object directly when no handle exists (the unit-test mock shape).
describe("@zeroship/bootstrap __platform routing (P9 PR 4)", () => {
  // Install a fake `globalThis.__zsDbPlatform` resolver that returns
  // `handle` for the given `db`, run `fn`, then restore the global.
  function withResolver<T>(
    db: unknown,
    handle: unknown,
    fn: () => T,
  ): T {
    const g = globalThis as unknown as { __zsDbPlatform?: (d: unknown) => unknown };
    const prev = g.__zsDbPlatform;
    g.__zsDbPlatform = (d: unknown) => (d === db ? handle : undefined);
    try {
      return fn();
    } finally {
      if (prev === undefined) delete g.__zsDbPlatform;
      else g.__zsDbPlatform = prev;
    }
  }

  test("resolveDbPlatform reads the handle via globalThis.__zsDbPlatform", () => {
    const db = { collection() { return {}; } };
    const handle = { registerModel: async () => {}, setMaskPolicy: async () => ({}) };
    const resolved = withResolver(db, handle, () => resolveDbPlatform(db));
    assert.equal(resolved, handle, "resolveDbPlatform must return the resolver's handle");
  });

  test("resolveDbPlatform prefers an explicitly-passed handle (no resolver call)", () => {
    const handle = { registerModel: async () => {}, setMaskPolicy: async () => ({}) };
    // No resolver installed — but `prefer` is honoured.
    const resolved = resolveDbPlatform({}, handle as never);
    assert.equal(resolved, handle);
  });

  test("resolveDbPlatform returns undefined when no resolver and no prefer", () => {
    const g = globalThis as unknown as { __zsDbPlatform?: unknown };
    const prev = g.__zsDbPlatform;
    delete g.__zsDbPlatform;
    try {
      assert.equal(resolveDbPlatform({}), undefined);
    } finally {
      if (prev !== undefined) g.__zsDbPlatform = prev;
    }
  });

  test("installSchema routes registerModel through the __platform handle, NOT env.db", async () => {
    const platformCalls: string[] = [];
    const envCalls: string[] = [];
    // `env.db` has NO registerModel (P9 PR 4 shape) — only collection +
    // transaction. A stray call to `env.registerModel` would be a bug.
    const env = {
      registerModel(name: string) { envCalls.push(name); return Promise.resolve(); },
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(_n: string) { return { async find() { return []; } }; },
    } as unknown as ZeroshipDb;
    const handle = {
      registerModel(name: string) { platformCalls.push(name); return Promise.resolve(); },
      setMaskPolicy: async () => ({}),
    };

    const { ready } = withResolver(env, handle, () =>
      installSchema({ users: { name: t.string().required() } }, env),
    );
    await ready;

    assert.deepEqual(platformCalls, ["users"], "registerModel must run on the __platform handle");
    assert.deepEqual(envCalls, [], "registerModel must NOT run on env.db when a handle exists");
  });

  test("installSchema honours an explicit options.platform handle", async () => {
    const platformCalls: string[] = [];
    const env = {
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(_n: string) { return { async find() { return []; } }; },
    } as unknown as ZeroshipDb;
    const handle = {
      registerModel(name: string) { platformCalls.push(name); return Promise.resolve(); },
      setMaskPolicy: async () => ({}),
    };
    const { ready } = installSchema(
      { posts: { title: t.string().required() } },
      env,
      { platform: handle },
    );
    await ready;
    assert.deepEqual(platformCalls, ["posts"]);
  });

  test("installSchema falls back to env.registerModel when no handle (mock shape)", async () => {
    const envCalls: string[] = [];
    const env = {
      registerModel(name: string) { envCalls.push(name); return Promise.resolve(); },
      transaction(cb: (raw: unknown) => unknown) { return cb(undefined); },
      collection(_n: string) { return { async find() { return []; } }; },
    } as unknown as ZeroshipDb;
    // No resolver, no options.platform — registration falls back to env.
    const g = globalThis as unknown as { __zsDbPlatform?: unknown };
    const prev = g.__zsDbPlatform;
    delete g.__zsDbPlatform;
    try {
      const { ready } = installSchema({ items: { name: t.string().required() } }, env);
      await ready;
    } finally {
      if (prev !== undefined) g.__zsDbPlatform = prev;
    }
    assert.deepEqual(envCalls, ["items"], "fallback to env.registerModel for the mock shape");
  });
});
