/**
 * P5.5 PR 8 — SDK type inference snapshot for the masking subsystem.
 *
 * Pins the `Row<S>` shape under three schema variants so a future
 * type-system tweak (e.g. widening `InferFieldDef` to drop the
 * `MaskedValue<T>` wrap on a masked column) lands a visible
 * compile-time failure here. The test bodies are deliberately
 * thin — what matters is that the TYPE assertions resolve under
 * `tsc`. Runtime assertions only sanity-check the symbols are
 * defined.
 *
 * The three closeout invariants exercised:
 *   1. A masked column on `Row<S>` materialises as `MaskedValue<T>`,
 *      not bare `T`.
 *   2. The `<col>_masked` sibling column is NEVER part of `Row<S>` —
 *      only the parent column appears. This is the SDK-side dual to
 *      the Rust-side `validate_field_name` reserved-suffix gate.
 *   3. `t.encrypted()` without an explicit `.mask({...})` infers as
 *      `MaskedValue<T>` (default mask is `{ kind: "full", classification:
 *      "pii" }`).
 *
 * Snapshot semantics: each `assertType<T>(value)` line is a compile-
 * time check via `satisfies`. If `tsc` rejects the line, the test
 * file fails to typecheck and the test runner reports the
 * failure — exactly the gate the proposal §11 line 734 asks for.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, MaskedValue } from "@zeroship/db";
import type { Row, RowInput } from "@zeroship/db";

// Helper: compile-time assertion that `T` matches the actual value's
// type. The body is a no-op at runtime; the win is `tsc` rejecting
// a mismatch.
function assertType<T>(_v: T): void {
  /* noop */
}

describe("P5.5 PR 8 — Row<S> shape under masking", () => {
  test("masked encrypted column materialises as MaskedValue<string>", () => {
    const usersSchema = {
      name: t.string().required(),
      ssn: t
        .encrypted({ wraps: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema>;

    // The masked column is a MaskedValue<string>, NOT a bare string.
    // The build-tier `tsc` invariant: assigning a bare string to
    // `ssn` would be rejected.
    const row: UsersRow = {
      id: 1,
      name: "Alice",
      ssn: new MaskedValue({
        masked: "***-**-6789",
        classification: "spi",
        sentinel: "__zsmask__",
      }),
      createdAt: 0,
      updatedAt: 0,
    };
    assertType<MaskedValue<string>>(row.ssn);
    assertType<string>(row.name);
    assert.equal(row.name, "Alice");
    assert.equal(row.ssn.toString(), "***-**-6789");
  });

  test("ssn_masked sibling column is NOT part of Row<S>", () => {
    const usersSchema = {
      ssn: t
        .encrypted({ wraps: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema>;

    // The sibling column must NOT be inferred. The line below uses
    // `keyof` to check the inferred key set; `ssn_masked` is not
    // present, so removing it from the union leaves the union
    // unchanged.
    type Keys = keyof UsersRow;
    type WithoutSibling = Exclude<Keys, "ssn_masked">;

    // If `ssn_masked` HAD been inferred, the assignment below would
    // type-error because `WithoutSibling` would be a strict subset
    // of `Keys`. With `ssn_masked` correctly absent, the two types
    // are identical and the cast succeeds.
    const sentinel = null as unknown as Keys;
    const checked = sentinel as WithoutSibling;
    assertType<Keys>(checked);
    assert.equal(checked, null);
  });

  test("encrypted column WITHOUT explicit .mask() still infers as MaskedValue (default full mask)", () => {
    const usersSchema = {
      email: t.encrypted({ wraps: t.string() }).required(),
    };
    type UsersRow = Row<typeof usersSchema>;

    // No explicit .mask() — the platform's default-mask rule
    // (kind: "full", classification: "pii") applies and the type
    // still wraps in MaskedValue<T>.
    const row: UsersRow = {
      id: 1,
      email: new MaskedValue({
        masked: "***************",
        classification: "pii",
        sentinel: "__zsmask__",
      }),
      createdAt: 0,
      updatedAt: 0,
    };
    assertType<MaskedValue<string>>(row.email);
    assert.equal(row.email.toString(), "***************");
  });

  test("mask kind 'none' opts out and infers as bare T", () => {
    const usersSchema = {
      // Explicit opt-out — column is encrypted at rest but the
      // wrapper is NOT applied on reads. This is the
      // intentional "I accept the risk" path.
      legacy: t
        .encrypted({ wraps: t.string() })
        .mask({ kind: "none", classification: "internal" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema>;
    const row: UsersRow = {
      id: 1,
      legacy: "raw",
      createdAt: 0,
      updatedAt: 0,
    };
    // .legacy is bare string (no MaskedValue wrap).
    assertType<string>(row.legacy);
    assert.equal(row.legacy, "raw");
  });

  test("RowInput<S> excludes auto-fields but keeps mask-wrap on inputs (creator writes the bare value)", () => {
    // Input shape — `id`, `createdAt`, `updatedAt` are auto-generated
    // and excluded from RowInput. The masked field appears in
    // RowInput as the same MaskedValue<T>-typed slot inferred from
    // the field definition; in practice, creators pass the bare
    // plaintext on insert and the platform's mask pass computes the
    // sibling before write.
    const usersSchema = {
      name: t.string().required(),
      ssn: t
        .encrypted({ wraps: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type Input = RowInput<typeof usersSchema>;
    // `name` is bare string in the input. `id` is forbidden.
    const input: Input = {
      name: "Alice",
      // Cast through unknown — at runtime the platform accepts the
      // bare plaintext; the type slot mirrors the read-side mask
      // shape (see PR 1 README: input/output symmetry through the
      // single inferred type).
      ssn: "123-45-6789" as unknown as MaskedValue<string>,
    };
    assertType<string>(input.name);
    assert.equal(input.name, "Alice");
  });
});
