/**
 * InferRow / InferRowInput / InferId — type-level helper checks.
 * These are compile-time assertions in disguise: the test bodies are
 * trivial, what matters is that the types resolve in tsc without errors.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";
import type { InferId, InferRow, InferRowInput } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

const native = {
  registerModel: () => Promise.resolve(),
  collection: (_name: string) => ({
    insert: async () => ({}),
  }),
} as unknown as NativeDb;

const Users = model(
  "users",
  {
    email: t.string().required().unique(),
    name: t.string().required(),
  },
  native,
);

const UsersWithDefault = model(
  "users_with_default",
  {
    email: t.string().required().unique(),
    role: t.string().required().default("user"),
  },
  native,
);

describe("Infer helpers", () => {
  test("InferRowInput pulls the RowInput shape off a Collection", () => {
    // Pure type-level check — at runtime we just confirm the symbol is
    // assignable. The TypeScript compiler enforces the shape match.
    type T = InferRowInput<typeof Users>;
    const x: T = { email: "a@b.com", name: "Alice" };
    assert.equal(x.email, "a@b.com");
  });

  test("InferRow pulls the Row shape (includes auto fields) off a Collection", () => {
    type T = InferRow<typeof Users>;
    const x: T = {
      id: "usr_001",
      email: "a@b.com",
      name: "Alice",
      created_at: 0,
      updated_at: 0,
      created_by: null,
      updated_by: null,
      version: 1,
      deleted_at: null,
    };
    assert.equal(x.id, "usr_001");
  });

  test("InferId pulls the branded Id<N> type off a Collection", () => {
    type T = InferId<typeof Users>;
    // `Id<N> = string & {...}` (typed_id, P7 PR 3) - the brand is a
    // phantom at runtime, but the base representation is a string, not
    // a number. `42 as T` compiled before this file was ever
    // typechecked; it does not after, because `number` and `string &
    // {...}` do not sufficiently overlap for a bare numeric cast.
    const x: T = "usr_042" as T;
    assert.equal(x, "usr_042");
  });

  test("required().default() fields are optional in RowInput", () => {
    type T = InferRowInput<typeof UsersWithDefault>;
    const x: T = { email: "a@b.com" };
    assert.equal(x.email, "a@b.com");
  });
});
