import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * Every procedure has an explicit auth policy, and every policy names a real
 * procedure.
 *
 * `src/server/config.ts` opens by claiming "Every procedure is listed
 * explicitly so the intended boundary is reviewable and a newly added RPC
 * cannot disappear into a build warning." That claim was FALSE when this test
 * was written: `groups.delete` had no entry. It had fallen through to the
 * fail-closed `auth: "user"` default, which is the correct answer for a
 * destructive admin operation, so nothing misbehaved and nothing ever would
 * have. The safety was the default, not the review.
 *
 * That is exactly the failure this file is for. A missing entry is invisible
 * precisely when the default happens to be right, and it stays invisible until
 * someone adds a procedure the default is WRONG for -- a public read, gated by
 * accident, or worse the reverse if the default ever changes.
 *
 * The counterpart direction matters too: a policy naming a procedure that no
 * longer exists is a boundary someone reviewed and believes is enforced, for
 * code that is gone.
 *
 * WHAT THIS DOES NOT CATCH: it compares NAMES only. It says nothing about
 * whether `auth: "anon"` is the right call for any given procedure -- that
 * judgement lives in the reviewer, and in `spec-drift.test.ts`, which pins the
 * anonymous COUNT so that widening the public surface has to be deliberate.
 */

const index = readFileSync(resolve(process.cwd(), "src/index.ts"), "utf8");
const config = readFileSync(resolve(process.cwd(), "src/server/config.ts"), "utf8");

/**
 * Procedure ids, read from the `{ id: "..." }` option object each `query` /
 * `mutation` / `action` / `stream` carries.
 *
 * Anchored to the closing paren of the wrapper rather than matching every
 * `{ id: "..." }` in the file. A naive match also picks up ordinary return
 * values -- `return { id: "__no_matching_typed_id__" }` at src/index.ts:342 is
 * one, and it made a first draft of this test report a phantom 90th procedure.
 */
function declaredProcedureIds(): string[] {
  const ids = [...index.matchAll(/\n\s*\{\s*id:\s*"([^"]+)"\s*\},?\s*\n\);/g)].map((m) => m[1]);
  return [...new Set(ids)].sort();
}

function policiedProcedureIds(): string[] {
  const ids = [...config.matchAll(/"rpc:([^"]+)"\s*:/g)].map((m) => m[1]);
  return [...new Set(ids)].sort();
}

describe("the RPC auth policy", () => {
  it("finds the procedures at all", () => {
    // Without this, both lists could be empty and every assertion below would
    // pass vacuously -- the regexes above are brittle by nature, since they
    // read source text rather than a module.
    expect(declaredProcedureIds().length, "no procedure ids parsed out of src/index.ts").toBeGreaterThan(50);
    expect(policiedProcedureIds().length, "no policy keys parsed out of config.ts").toBeGreaterThan(50);
  });

  it("covers every declared procedure", () => {
    const missing = declaredProcedureIds().filter((id) => !policiedProcedureIds().includes(id));
    expect(
      missing,
      `these procedures have no entry in src/server/config.ts, so their boundary is whatever the ` +
        `default happens to be rather than something anyone chose: ${missing.join(", ")}`,
    ).toEqual([]);
  });

  it("names no procedure that does not exist", () => {
    const orphaned = policiedProcedureIds().filter((id) => !declaredProcedureIds().includes(id));
    expect(
      orphaned,
      `src/server/config.ts declares a policy for procedures that are not defined, which reads as ` +
        `an enforced boundary over code that is gone: ${orphaned.join(", ")}`,
    ).toEqual([]);
  });
});
