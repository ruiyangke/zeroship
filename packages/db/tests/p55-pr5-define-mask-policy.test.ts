import { afterEach, test } from "node:test";
import assert from "node:assert/strict";
import { env } from "zeroship";
import { defineMaskPolicy } from "../src/index.js";

const mutableEnv = env as unknown as Record<string, unknown>;
const original = mutableEnv.db;
afterEach(() => { if (original === undefined) delete mutableEnv.db; else mutableEnv.db = original; });

test("forwards the declaration to its native database receiver", () => {
  const declarations: unknown[] = [];
  const db = { declareMaskPolicy(policy: unknown) { assert.equal(this, db); declarations.push(policy); } };
  mutableEnv.db = db;
  const policy = { support: ["pii"] } as const;
  defineMaskPolicy(policy);
  assert.deepEqual(declarations, [policy]);
});

test("preserves native declaration failures", () => {
  const error = Object.assign(new Error("policy sealed"), { code: "MASK_POLICY_IMMUTABLE" });
  mutableEnv.db = { declareMaskPolicy() { throw error; } };
  assert.throws(() => defineMaskPolicy({ support: ["pii"] }), caught => caught === error);
});

test("requires the host binding", () => {
  for (const value of [undefined, {}, { declareMaskPolicy: true }]) {
    mutableEnv.db = value;
    assert.throws(() => defineMaskPolicy({}), { code: "DB_STARTUP_BINDING_REQUIRED" });
  }
});
