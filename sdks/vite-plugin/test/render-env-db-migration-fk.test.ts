// Generated reference builders retain the descriptor's target collection and column.
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  renderGeneratedEnvDb,
  type RuntimeDescriptor,
} from "../src/gen-types/render-env-db.js";

function descriptorWithMigrationFk(): RuntimeDescriptor {
  return {
    collections: {
      users: {
        fields: {
          account_key: { type: "string", required: true, primaryKey: true },
          email: { type: "string", required: true },
        },
      },
      todos: {
        fields: {
          userId: { type: "string", required: true, refTarget: "users", refColumn: "account_key" },
          title: { type: "string", required: true },
        },
      },
    },
  } as unknown as RuntimeDescriptor;
}

describe("render-env-db — migration-declared foreign keys", () => {
  test("a string field carrying refTarget generates as a relation", () => {
    const out = renderGeneratedEnvDb(descriptorWithMigrationFk());

    // The generated source must carry the relation, by whatever spelling the
    // renderer uses for it. Asserting on `refTarget` reaching the output is
    // the property that matters - it is what `ExtractRefTarget` reads, and
    // therefore what decides whether the joined field types as the target row
    // or as `null`.
    const userIdLine = out
      .split("\n")
      .find((l) => l.includes("userId:"));
    assert.ok(userIdLine, "the generated schema must declare userId");
    assert.match(userIdLine, /column: "account_key"/);
    assert.ok(
      /t\.ref\(|references\(/.test(userIdLine!),
      `a migration FK must generate as a relation, not a bare string; got: ${userIdLine!.trim()}`,
    );
    assert.ok(
      userIdLine!.includes("users"),
      `the relation must name its target collection; got: ${userIdLine!.trim()}`,
    );
  });

  test("a plain string field without refTarget stays a plain string", () => {
    // The control. Without this, a fix that renders EVERY string as a relation
    // would satisfy the assertion above while corrupting every ordinary column.
    const out = renderGeneratedEnvDb(descriptorWithMigrationFk());
    const titleLine = out.split("\n").find((l) => l.includes("title:"));
    assert.ok(titleLine, "the generated schema must declare title");
    assert.ok(
      !/t\.ref\(|references\(/.test(titleLine!),
      `a field with no refTarget must not become a relation; got: ${titleLine!.trim()}`,
    );
  });
});
