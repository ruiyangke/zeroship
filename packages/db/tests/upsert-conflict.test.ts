import test from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import type { Collection } from "../../../crates/zeroship-data-v8/js/runtime/collection";
import type { TxCollection } from "../src/db-types";
import type { NativeDb } from "../src/native";
import { installSchemaForTest } from "./_install-helper";

async function conflictTypes(collection: Collection<{ email: string }>, tx: TxCollection<{ email: string }>) {
  await collection.upsert({ email: "alice@example.com" }, { conflictFields: ["email"] });
  await tx.upsert({ email: "alice@example.com" }, { conflictFields: ["email"] });
  // @ts-expect-error platform identities are not application-owned conflict keys
  await collection.upsert({ email: "alice@example.com" }, { conflictFields: ["id"] });
  // @ts-expect-error transaction collections enforce the same conflict-key type
  await tx.upsert({ email: "alice@example.com" }, { conflictFields: ["id"] });
}
void conflictTypes;

test("upsert validates conflict keys before calling the native collection", async () => {
  const calls: unknown[] = [];
  const native = {
    collection() {
      return {
        async upsert(doc: Record<string, unknown>, options: unknown) {
          calls.push({ doc, options });
          return { ...doc, id: "cont_native" };
        },
      };
    },
  } as unknown as NativeDb;
  const db = installSchemaForTest({
    contacts: { emailAddress: t.string().required(), name: t.string() },
  }, {
    native,
    naming: {
      toColumn: (field) => field === "emailAddress" ? "email_address" : field === "createdAt" ? "created_at" : field,
      toField: (column) => column === "email_address" ? "emailAddress" : column,
    },
  });
  for (const conflictFields of [
    [], ["id"], ["createdAt"], ["version"], ["emailAddress", 1],
    ["emailAddress", null], ["emailAddress", "emailAddress"],
    ["unknown"], ["name"], "emailAddress", null,
  ]) {
    const result = await db.contacts.upsert(
      { emailAddress: "alice@example.com" }, { conflictFields } as never,
    );
    assert.ok(result.error, JSON.stringify(conflictFields));
    assert.deepEqual(calls, []);
  }
  const row = { emailAddress: "alice@example.com" };
  const options = { conflictFields: ["emailAddress"] };
  const result = await db.contacts.upsert(row, options as never);
  assert.equal(result.error, null);
  assert.deepEqual(calls, [{ doc: { email_address: "alice@example.com" }, options: { conflictFields: ["email_address"] } }]);
  assert.deepEqual(row, { emailAddress: "alice@example.com" });
  assert.deepEqual(options, { conflictFields: ["emailAddress"] });
});
