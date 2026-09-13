import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { normalizeUserModule } from "../src/normalize.js";

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

  test("does not read or surface default.schema", () => {
    const def: Record<string, unknown> = {};
    Object.defineProperty(def, "schema", {
      get() {
        throw new Error("default.schema must not be read");
      },
    });
    const out = normalizeUserModule({ default: def });
    assert.equal("schema" in out, false);
  });
});
