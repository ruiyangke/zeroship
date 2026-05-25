/**
 * Infer / InferRow / InferRowInput / InferId — type-level helper checks.
 * These are compile-time assertions in disguise: the test bodies are
 * trivial, what matters is that the types resolve in tsc without errors.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";
import type { Infer, InferId, InferRow, InferRowInput } from "@zeroship/db";

const native = {
  registerModel: () => Promise.resolve(),
  collection: (_name: string) => ({
    insert: async () => ({}),
  }),
} as unknown as ZeroshipDb;

const Users = model(
  "users",
  {
    email: t.string().required().unique(),
    name: t.string().required(),
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
      id: 1,
      email: "a@b.com",
      name: "Alice",
      created_at: 0,
      updated_at: 0,
    };
    assert.equal(x.id, 1);
  });

  test("InferId pulls the branded Id<N> type off a Collection", () => {
    type T = InferId<typeof Users>;
    // The brand is a phantom — at runtime it's a number.
    const x: T = 42 as T;
    assert.equal(x, 42);
  });

  test("Infer<typeof col.RowInput> identity-resolves", () => {
    type T = Infer<typeof Users.RowInput>;
    const x: T = { email: "a@b.com", name: "Alice" };
    assert.equal(x.name, "Alice");
  });
});
