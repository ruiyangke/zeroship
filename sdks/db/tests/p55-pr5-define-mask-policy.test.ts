/**
 * **P5.5 PR 5** — `defineMaskPolicy()` validation + pending-slot drain
 * contract.
 *
 * Pins the SDK-side behaviour the runtime depends on:
 *
 *   1. A well-formed policy parks in the pending slot.
 *   2. `_flushPendingMaskPolicy()` drains exactly once — second drain
 *      yields `null`.
 *   3. Invalid classifications refuse at declare-time with
 *      `INVALID_MASK_CLASSIFICATION` (matched by the Rust-side
 *      validation; belt-and-braces).
 *   4. Shape errors (non-array role value, non-object policy) refuse
 *      with `INVALID_MASK_POLICY_SHAPE`.
 *   5. The stored policy is a defensive clone — mutating the caller's
 *      array after declare does not bleed into the pending slot.
 */

import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";

import { defineMaskPolicy } from "@zeroship/db";
import { _flushPendingMaskPolicy, _peekPendingMaskPolicy } from "@zeroship/db/internal";

describe("P5.5 PR 5 — defineMaskPolicy() validation", () => {
  beforeEach(() => {
    // Drain any pending policy left by a previous test — the
    // module-local slot is process-global.
    _flushPendingMaskPolicy();
  });

  test("well-formed policy parks in the pending slot", () => {
    defineMaskPolicy({
      admin: ["public", "pii", "spi", "phi", "pci", "internal"],
      support: ["public", "pii"],
      user: ["public"],
      auto: ["public", "pii", "spi", "phi", "pci", "internal"],
    });
    const parked = _peekPendingMaskPolicy();
    assert.ok(parked, "pending slot must hold the declared policy");
    assert.deepEqual(parked!.admin, [
      "public",
      "pii",
      "spi",
      "phi",
      "pci",
      "internal",
    ]);
    assert.deepEqual(parked!.user, ["public"]);
  });

  test("flush drains the slot exactly once", () => {
    defineMaskPolicy({ user: ["public"] });
    const first = _flushPendingMaskPolicy();
    assert.ok(first, "first flush returns the policy");
    assert.deepEqual(first!.user, ["public"]);

    const second = _flushPendingMaskPolicy();
    assert.equal(second, null, "second flush returns null");
  });

  test("re-declaring overwrites the pending slot", () => {
    defineMaskPolicy({ user: ["public"] });
    defineMaskPolicy({ admin: ["pii"] });
    const parked = _peekPendingMaskPolicy();
    assert.ok(parked);
    assert.equal(
      parked!.user,
      undefined,
      "previous declaration must be replaced wholesale",
    );
    assert.deepEqual(parked!.admin, ["pii"]);
  });

  test("flush yields null when no policy was declared", () => {
    assert.equal(_flushPendingMaskPolicy(), null);
  });

  test("rejects unknown classification with invalid_mask_classification", () => {
    assert.throws(
      () => defineMaskPolicy({ admin: ["badclass"] as never }),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "INVALID_MASK_CLASSIFICATION",
    );
    // Slot must not be poisoned with a partial set.
    assert.equal(_peekPendingMaskPolicy(), null);
  });

  test("rejects empty-string classification", () => {
    assert.throws(
      () => defineMaskPolicy({ user: ["" as never] }),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "INVALID_MASK_CLASSIFICATION",
    );
  });

  test("rejects non-array classifications value", () => {
    assert.throws(
      () =>
        defineMaskPolicy({
          admin: "pii" as unknown as readonly never[],
        }),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "INVALID_MASK_POLICY_SHAPE",
    );
  });

  test("rejects non-object policy", () => {
    assert.throws(
      () => defineMaskPolicy(null as unknown as Record<string, never>),
      (e: unknown) =>
        e instanceof Error &&
        (e as Error & { code?: string }).code === "INVALID_MASK_POLICY_SHAPE",
    );
  });

  test("declared policy is defensively cloned", () => {
    const userClassifications: ("public" | "pii")[] = ["public"];
    defineMaskPolicy({ user: userClassifications });
    // Mutate the caller's array after declare.
    userClassifications.push("pii");
    const parked = _peekPendingMaskPolicy();
    assert.deepEqual(
      parked!.user,
      ["public"],
      "post-declare mutation must NOT bleed into the pending slot",
    );
  });

  test("auto role is accepted with the full classification set", () => {
    defineMaskPolicy({
      auto: ["public", "pii", "spi", "phi", "pci", "internal"],
    });
    const parked = _peekPendingMaskPolicy();
    assert.ok(parked);
    assert.equal(parked!.auto.length, 6);
  });

  test("empty classification array is valid (role has no privileges)", () => {
    // An empty array is a legitimate way to express "this role exists
    // but cannot unmask anything" — useful for explicit auto-restriction.
    defineMaskPolicy({ auto: [] });
    const parked = _peekPendingMaskPolicy();
    assert.ok(parked);
    assert.deepEqual(parked!.auto, []);
  });
});
