/**
 * Regression — preserve a native Error's `.code` end-to-end.
 *
 * The native side of the runtime throws Errors carrying a structured
 * `.code` (e.g. `"migration_already_running"`, `"unique_violation"`).
 * Earlier the SDK's `_run → catch → toResultError → mapNativeError(msg)`
 * path rebuilt the Error from the message alone, dropping `.code`. These
 * tests cover the reachable call sites — insert, update, query (find),
 * plus the fallback for uncoded inputs.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";
import { mapNativeError } from "../src/errors.js";

type AnyRec = Record<string, unknown>;

function makeFailingNative(err: Error) {
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async insert(_doc: AnyRec) { throw err; },
        async update(_f: AnyRec, _u: AnyRec) { throw err; },
        async find(_f: AnyRec, _o: AnyRec) { throw err; },
      };
    },
  };
  return native as unknown as ZeroshipDb;
}

describe("native error .code preservation", () => {
  test("insert: coded native error reaches caller via result.error.code", async () => {
    const native = makeFailingNative(
      Object.assign(new Error("migration in flight"), {
        code: "migration_already_running",
      }),
    );
    const Users = model("users", { name: t.string().required() }, native);
    const { error } = await Users.insert({ name: "Alice" });
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "migration_already_running");
    assert.equal(error?.message, "migration in flight");
  });

  test("update: coded native error reaches caller", async () => {
    const native = makeFailingNative(
      Object.assign(new Error("unique violation"), { code: "unique_violation" }),
    );
    const Users = model("users", { name: t.string().required() }, native);
    const { error } = await Users.update({ id: 1 }, { $set: { name: "Bob" } });
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "unique_violation");
  });

  test("find: coded native error reaches caller through Query._exec", async () => {
    const native = makeFailingNative(
      Object.assign(new Error("permission denied"), { code: "permission_denied" }),
    );
    const Users = model("users", { name: t.string().required() }, native);
    const { error } = await Users.find({});
    assert.ok(error);
    assert.equal((error as Error & { code?: string }).code, "permission_denied");
    assert.equal(error?.message, "permission denied");
  });

  test("bare string fallback becomes a plain Error with no synthetic code", () => {
    const out = mapNativeError("violated unique constraint x_email_idx");
    assert.equal(out.message, "violated unique constraint x_email_idx");
    assert.equal(typeof (out as Error & { code?: unknown }).code, "undefined");
  });

  test("uncoded Error fallback preserves identity", () => {
    const original = new Error("duplicate key value");
    const out = mapNativeError(original);
    assert.strictEqual(out, original);
  });

  test("pass-through: Error with .code is returned unchanged (same identity)", () => {
    const original = Object.assign(new Error("x"), { code: "x_code" });
    const out = mapNativeError(original);
    assert.strictEqual(out, original);
  });
});
