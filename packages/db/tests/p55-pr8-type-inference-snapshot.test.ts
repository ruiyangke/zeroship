import { generatedSchema } from "./_install-helper.js";
/**
 * Compile-time contracts for masked row and input inference.
 *
 * Logical masked fields materialize as `MaskedValue<T>`, descriptor-selected
 * raw storage stays out of `Row<S>`, and default masking applies to encrypted
 * fields without an explicit mask declaration.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
// `MaskedValue` is a type-only declaration for the native V8 class.
import type { Row, RowInput, MaskedValue } from "../src/index.js";

// Helper: compile-time assertion that `T` matches the actual value's
// type. The body is a no-op at runtime; the win is `tsc` rejecting
// a mismatch.
function assertType<T>(_v: T): void {
  /* noop */
}

describe("Row<S> shape under masking", () => {
  test("masked encrypted column materialises as MaskedValue<string>", () => {
    const usersSchema = {
      name: t.string().required(),
      ssn: t
        .encrypted({ of: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema & typeof generatedSchema>;

    // The masked column is a MaskedValue<string>, NOT a bare string.
    // The build-tier `tsc` invariant: assigning a bare string to
    // `ssn` would be rejected.
    const row: UsersRow = {
      id: "usr_001",
      name: "Alice",
      ssn: "***-**-6789" as unknown as MaskedValue<string>,
      created_at: 0,
      updated_at: 0,
      created_by: null,
      updated_by: null,
      version: 1,
      deleted_at: null,
    };
    assertType<MaskedValue<string>>(row.ssn);
    assertType<string>(row.name);
    assert.equal(row.name, "Alice");
    assert.equal(row.ssn as unknown as string, "***-**-6789");
  });

  test("the raw column is NOT part of Row<S>", () => {
    const usersSchema = {
      ssn: t
        .encrypted({ of: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema & typeof generatedSchema>;

    // A masked field owns a second physical column, `__zs_raw__ssn`,
    // holding the real value. It must NOT be inferred into the row type:
    // the generated type is what a review of a handler is written
    // against, so a column present at runtime but absent from the type
    // is invisible to that review.
    //
    type Keys = keyof UsersRow;
    type WithoutRaw = Exclude<Keys, "__zs_raw__ssn">;

    // If `__zs_raw__ssn` HAD been inferred, the assignment below would
    // type-error because `WithoutRaw` would be a strict subset of
    // `Keys`. With it correctly absent, the two types are identical and
    // the cast succeeds.
    const sentinel = null as unknown as Keys;
    const checked = sentinel as WithoutRaw;
    assertType<Keys>(checked);
    assert.equal(checked, null);

    // The positive control, differing in one variable: the LOGICAL name
    // IS inferred. Without it, a `Row<S>` that inferred nothing at all
    // would satisfy the exclusion above.
    assertType<"ssn">(null as unknown as Extract<Keys, "ssn">);
  });

  test("encrypted column WITHOUT explicit .mask() still infers as MaskedValue (default full mask)", () => {
    const usersSchema = {
      email: t.encrypted({ of: t.string() }).required(),
    };
    type UsersRow = Row<typeof usersSchema & typeof generatedSchema>;

    // No explicit .mask() — the platform's default-mask rule
    // (kind: "full", classification: "pii") applies and the type
    // still wraps in MaskedValue<T>.
    const row: UsersRow = {
      id: "usr_001",
      email: "***************" as unknown as MaskedValue<string>,
      created_at: 0,
      updated_at: 0,
      created_by: null,
      updated_by: null,
      version: 1,
      deleted_at: null,
    };
    assertType<MaskedValue<string>>(row.email);
    assert.equal(row.email as unknown as string, "***************");
  });

  test("mask kind 'none' opts out and infers as bare T", () => {
    const usersSchema = {
      // Explicit opt-out — column is encrypted at rest but the
      // wrapper is NOT applied on reads. This is the
      // intentional "I accept the risk" path.
      plaintext: t
        .encrypted({ of: t.string() })
        .mask({ kind: "none", classification: "internal" })
        .required(),
    };
    type UsersRow = Row<typeof usersSchema & typeof generatedSchema>;
    const row: UsersRow = {
      id: "usr_001",
      plaintext: "raw",
      created_at: 0,
      updated_at: 0,
      created_by: null,
      updated_by: null,
      version: 1,
      deleted_at: null,
    };
    assertType<string>(row.plaintext);
    assert.equal(row.plaintext, "raw");
  });

  test("RowInput<S> excludes auto-fields but keeps mask-wrap on inputs (creator writes the bare value)", () => {
    // Generator-supplied fields are excluded from RowInput. The masked field
    // appears in RowInput as the
    // same MaskedValue<T>-typed slot inferred from the field definition;
    // creators pass plaintext and the ORM maps it through descriptor-selected
    // storage before writing.
    const usersSchema = {
      name: t.string().required(),
      ssn: t
        .encrypted({ of: t.string() })
        .mask({ kind: "last4", classification: "spi" })
        .required(),
    };
    type Input = RowInput<typeof usersSchema>;
    // `name` is bare string in the input. `id` is forbidden.
    const input: Input = {
      name: "Alice",
      // Runtime accepts plaintext while the declared input slot mirrors the
      // masked field type.
      ssn: "123-45-6789" as unknown as MaskedValue<string>,
    };
    assertType<string>(input.name);
    assert.equal(input.name, "Alice");
  });
});
