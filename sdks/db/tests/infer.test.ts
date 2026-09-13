import { generatedSchema } from "./_install-helper.js";
/**
 * InferRow / InferRowInput / InferId — type-level helper checks.
 * These are compile-time assertions in disguise: the test bodies are
 * trivial, what matters is that the types resolve in tsc without errors.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { schema, t } from "@zeroship/db";
import type { Db, Id, InferId, InferRow, InferRowInput } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

const native = {
  collection: (_name: string) => ({
    insert: async () => ({}),
  }),
} as unknown as NativeDb;

const Users = model(
  "users",
  { ...generatedSchema,
    email: t.string().required().unique(),
    name: t.string().required(),
  },
  native,
);

const UsersWithDefault = model(
  "users_with_default",
  { ...generatedSchema,
    email: t.string().required().unique(),
    role: t.string().required().default("user"),
  },
  native,
);

describe("Infer helpers", () => {
  test("generated collections retain their schema and identity through inference", () => {
    const schemas = {
      users: schema({
        id: t.string().required().primaryKey().assigned({ by: "typedId", on: "insert" }),
        email: t.string().required(),
      }),
      todos: schema({
        id: t.number().required().primaryKey(),
        userId: t.ref("users").required(),
      }),
      workspaces: schema({
        id: t.string().required().primaryKey(),
      }),
    };
    type Users = Db<typeof schemas>["users"];
    type Todos = Db<typeof schemas>["todos"];
    const row: InferRow<Users> = { id: "user_example", email: "test@example.com" };
    const input: InferRowInput<Users> = { email: row.email };
    const userId: InferId<Users> = "user_example" as Id<"users", string>;
    const todoId: InferId<Todos> = 7 as Id<"todos", number>;
    const workspaceId: InferId<Db<typeof schemas>["workspaces"]> = "work_example" as Id<"workspaces", string>;
    assert.equal(input.email, row.email);
    assert.equal(userId, row.id);
    assert.equal(todoId, 7);

    function rejectionControls() {
      // @ts-expect-error The inferred row preserves its field types.
      const invalidRow: InferRow<Users> = { id: 7, email: "test@example.com" };
      // @ts-expect-error Assigned identities are excluded from insert input.
      const invalidInput: InferRowInput<Users> = { id: "user_supplied", email: row.email };
      // @ts-expect-error Collection identity brands cannot cross tables.
      const wrongTable: InferId<Users> = workspaceId;
      // @ts-expect-error Text identities cannot become numeric identities.
      const wrongStorage: InferId<Todos> = userId;
      return { invalidRow, invalidInput, wrongTable, wrongStorage };
    }
    void rejectionControls;
  });

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
