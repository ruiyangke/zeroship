/**
 * The fields `model()` injects AFTER `normalizeSchema` has already run.
 *
 * `normalizeSchema` is where the operator charter stamps `assign` onto a
 * platform column. Anything injected after it returns bypasses that stamp: it
 * carries no `assign`, so `validateDoc` falls through to the `default` arm,
 * which MATERIALISES the value into the document handed to the native op.
 *
 * For `deletedAt` that is harmless - it declares no default, so there is
 * nothing to materialise. For `version` it is not: it declares `default: 1`,
 * which is the DDL seed, and the design is explicit that this key must never
 * reach `build_upsert`. There the generic upsert loop emits
 * `"version" = EXCLUDED."version"` while the auto-bump emits a second
 * assignment to the same column, and PostgreSQL refuses two assignments to one
 * column in a single `DO UPDATE SET`.
 *
 * That asymmetry is why this went unnoticed: the design named `deletedAt`, the
 * twin that cannot bite.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { model } from "@zeroship/bootstrap/install-schema";
import { t } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

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

describe("post-normalization injections", () => {
  test("an insert does not carry the version seed to the native op", async () => {
    const { native, seen } = recordingNative();
    // Positional: (name, schema, native, naming, softDelete, versioning, indexes)
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
      undefined,
      false,
      true,
    );

    await Posts.insert({ title: "hello" } as never);

    assert.equal(seen.length, 1, "the stub must have received one document");
    const doc = seen[0] as Record<string, unknown>;
    assert.ok(
      !("version" in doc),
      `version is platform-assigned, so the DDL seed must not be written into ` +
        `the insert; got ${JSON.stringify(doc)}`,
    );
  });

  test("soft delete does not carry a deletedAt to the native op", async () => {
    const { native, seen } = recordingNative();
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
      undefined,
      true,
      false,
    );

    await Posts.insert({ title: "hello" } as never);

    const doc = seen[0] as Record<string, unknown>;
    assert.ok(
      !("deletedAt" in doc),
      `deletedAt is platform-assigned; got ${JSON.stringify(doc)}`,
    );
  });

  // The control. A change that dropped every field would satisfy both cases
  // above, so prove the creator's own column still arrives.
  test("the creator's own field still reaches the native op", async () => {
    const { native, seen } = recordingNative();
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
      undefined,
      false,
      true,
    );

    await Posts.insert({ title: "hello" } as never);

    const doc = seen[0] as Record<string, unknown>;
    assert.equal(doc.title, "hello", "the creator's value must survive");
  });
});
