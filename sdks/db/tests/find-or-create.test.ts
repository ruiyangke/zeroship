/**
 * findOrCreate — collection-level unit tests with a mock NativeDb. The
 * mock exposes a `.collection(name)` factory and a `findOrCreate` method
 * that records its arguments so we can assert wiring (conflict-field
 * inference, doc merging) without a live Postgres.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";

type AnyRec = Record<string, unknown>;

function makeMockNative(returnRows: { row: AnyRec; created: boolean }[]) {
  const calls: { doc: AnyRec; opts: { conflictFields: string[] } }[] = [];
  let i = 0;
  const native = {
    registerModel: () => Promise.resolve(),
    collection(_name: string) {
      return {
        async findOrCreate(doc: AnyRec, opts: { conflictFields: string[] }) {
          calls.push({ doc, opts });
          const next = returnRows[i++] ?? returnRows[returnRows.length - 1];
          return next;
        },
      };
    },
  };
  return { native: native as unknown as ZeroshipDb, calls };
}

describe("findOrCreate", () => {
  test("created=true is returned when a fresh row is inserted", async () => {
    const { native, calls } = makeMockNative([
      { row: { id: 1, email: "a@b.com", name: "Alice" }, created: true },
    ]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );
    const { data, error } = await Users.findOrCreate(
      { email: "a@b.com" },
      { email: "a@b.com", name: "Alice" },
    );
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(data.created, true);
    assert.equal(data.row.id, 1);
    assert.equal(data.row.name, "Alice");
    // conflictFields auto-inferred from the unique single-key filter.
    assert.deepEqual(calls[0].opts.conflictFields, ["email"]);
    // Filter merged into create payload — both fields make it through.
    assert.equal(calls[0].doc.email, "a@b.com");
    assert.equal(calls[0].doc.name, "Alice");
  });

  test("created=false is returned when an existing row is found", async () => {
    const { native } = makeMockNative([
      { row: { id: 7, email: "a@b.com", name: "Alice" }, created: false },
    ]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );
    const { data, error } = await Users.findOrCreate(
      { email: "a@b.com" },
      { email: "a@b.com", name: "Alice" },
    );
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(data.created, false);
    assert.equal(data.row.id, 7);
  });

  test("multi-key filter without explicit conflictFields rejects with TypeError", async () => {
    const { native } = makeMockNative([
      { row: { id: 1 }, created: true },
    ]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );
    const { error } = await Users.findOrCreate(
      { email: "a@b.com", name: "Alice" },
      { email: "a@b.com", name: "Alice" },
    );
    assert.ok(error);
    assert.match(error.message, /conflictFields is required/);
  });

  test("non-unique single-key filter rejects without explicit conflictFields", async () => {
    const { native } = makeMockNative([
      { row: { id: 1 }, created: true },
    ]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );
    const { error } = await Users.findOrCreate(
      { name: "Alice" },
      { email: "a@b.com", name: "Alice" },
    );
    assert.ok(error);
    assert.match(error.message, /not declared .unique\(\)/);
  });

  test("explicit conflictFields:[] is treated as auto-infer; multi-key filter rejects with a clear conflictFields message", async () => {
    // Gap M from the robustness audit — the SDK collapses an explicit
    // empty array onto the auto-infer branch (`length === 0` short-circuit
    // in collection.ts:555). With a 2-key filter the inference cannot
    // disambiguate which column ON CONFLICT should target, so the SDK's
    // TypeError fires before reaching native's defensive reject.
    const { native } = makeMockNative([{ row: { id: 1 }, created: true }]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
      },
      native,
    );
    const { data, error } = await Users.findOrCreate(
      { email: "a@b.com", name: "Alice" },
      { email: "a@b.com", name: "Alice" },
      { conflictFields: [] as never },
    );
    assert.equal(data, null);
    assert.ok(error);
    assert.match(error.message, /conflictFields/);
  });

  test("empty create object with required fields rejects naming the missing field", async () => {
    // Gap EE — `validateDoc` runs over the merged `{ ...create, ...filter }`
    // payload. With a single-key filter and `create: {}`, every other
    // required field has no value; the ValidationError message must name
    // the missing field so the AI builder can self-correct.
    const { native } = makeMockNative([{ row: { id: 1 }, created: true }]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required(),
        age: t.number().required(),
      },
      native,
    );
    const { data, error } = await Users.findOrCreate(
      { email: "a@b.com" },
      {} as unknown as { email: string; name: string; age: number },
    );
    assert.equal(data, null);
    assert.ok(error);
    // Should name at least one missing required field.
    assert.match(error.message, /name|age/);
    assert.match(error.message, /required/);
  });

  test("explicit conflictFields override the auto-inference", async () => {
    const { native, calls } = makeMockNative([
      { row: { id: 1, email: "a@b.com", name: "Alice" }, created: true },
    ]);
    const Users = model(
      "users",
      {
        email: t.string().required().unique(),
        name: t.string().required().unique(),
      },
      native,
    );
    await Users.findOrCreate(
      { email: "a@b.com", name: "Alice" },
      { email: "a@b.com", name: "Alice" },
      { conflictFields: ["email"] },
    );
    assert.deepEqual(calls[0].opts.conflictFields, ["email"]);
  });
});
