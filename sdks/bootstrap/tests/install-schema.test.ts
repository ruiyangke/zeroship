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
      assert.equal(err.code, "ref_target_not_found");
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
