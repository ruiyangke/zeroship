// Chain-method aliasing regression.
//
// `TypeBuilder`'s chain methods currently assign to `this._def` and return
// `this`, so builders derived from a shared base describe the SAME mutable
// field definition. Two consequences, both silent:
//
//   1. The later call's facet appears on the earlier call's builder, and its
//      declared type no longer matches its runtime shape — a builder whose
//      type says optional is `required` at runtime.
//   2. The base itself is dirty for every subsequent use.
//
// The generated `env.db.ts` never hits this (every field chains off a fresh
// factory call), which is why it survived; hand-written and shared schemas do.
//
// Snapshots are taken AFTER both chains run: that is what exposes the shared
// `_def`, because the last write wins for every reader.
import assert from "node:assert/strict";
import { test } from "node:test";

import { t } from "../src/index.js";

test("scalar chain methods do not alias facets between siblings", () => {
  const base = t.string();
  const a = base.required();
  const b = base.max(10);

  const defA = a.toFieldDef();
  const defB = b.toFieldDef();
  const defBase = base.toFieldDef();

  assert.equal(defA.required, true, "a is the builder that asked for required");
  assert.equal(defA.max, undefined, "a must not acquire b's max");

  assert.equal(defB.max, 10, "b is the builder that asked for max");
  assert.equal(defB.required, undefined, "b must not inherit a's required");

  assert.equal(defBase.required, undefined, "the base must stay unmodified");
  assert.equal(defBase.max, undefined, "the base must stay unmodified");
});

test("references() does not smear foreign-key metadata between siblings", () => {
  const base = t.string();
  const refA = base.references("users_a", { relation: "owner" });
  const refB = base.references("users_b");

  const defA = refA.toFieldDef();
  const defB = refB.toFieldDef();

  assert.equal(defA.refTarget, "users_a");
  assert.equal(defA.relation, "owner");
  assert.equal(defB.refTarget, "users_b");
  assert.equal(defB.relation, undefined, "b must not inherit a's relation name");
});
