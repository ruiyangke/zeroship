/** Collection options do not invent columns or assignment metadata. */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../../../crates/zeroship-data-v8/js/runtime/collection.js";
import { t } from "../src/index.js";
import { fieldsOf } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";

const postsSchema = {
  id: t.string().required().primaryKey().assigned({ by: "typedId", on: "insert" }),
  title: t.string().required(),
};

/** A native stub that records the document each insert receives. */
function recordingNative(): { native: NativeDb; seen: unknown[] } {
  const seen: unknown[] = [];
  const native = {
    collection: (_name: string) => ({
      insert: async (doc: unknown) => {
        seen.push(doc);
        return {};
      },
    }),
  } as unknown as NativeDb;
  return { native, seen };
}

describe("normalization preserves declared columns", () => {
  test("an insert does not carry the version seed to the native op", async () => {
    const { native, seen } = recordingNative();
    const Posts = new Collection<typeof postsSchema>(
      "posts",
      fieldsOf(postsSchema),
      native,
    );

    await Posts.insert({ title: "hello" } as never);

    assert.equal(seen.length, 1, "the stub must have received one document");
    const doc = seen[0] as Record<string, unknown>;
    assert.ok(
      !("version" in doc),
      `undeclared version must not reach the insert: ${JSON.stringify(doc)}`,
    );
  });

  test("soft delete does not carry a deletedAt to the native op", async () => {
    const { native, seen } = recordingNative();
    const Posts = new Collection<typeof postsSchema>(
      "posts",
      fieldsOf(postsSchema),
      native,
    );

    await Posts.insert({ title: "hello" } as never);

    const doc = seen[0] as Record<string, unknown>;
    assert.ok(
      !("deletedAt" in doc),
      `undeclared deletedAt must not reach the insert: ${JSON.stringify(doc)}`,
    );
  });

  // The control. A change that dropped every field would satisfy both cases
  // above, so prove the creator's own column still arrives.
  test("the creator's own field still reaches the native op", async () => {
    const { native, seen } = recordingNative();
    const Posts = new Collection<typeof postsSchema>(
      "posts",
      fieldsOf(postsSchema),
      native,
    );

    await Posts.insert({ title: "hello" } as never);

    const doc = seen[0] as Record<string, unknown>;
    assert.equal(doc.title, "hello", "the creator's value must survive");
  });
});
