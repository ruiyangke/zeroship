import { describe, test } from "node:test";
import assert from "node:assert/strict";
import {
  decimal,
  t,
  type Decimal,
  type Filter,
  type RowInput,
  type SortSpec,
  type UpdateExpression,
} from "../src/index.js";
import { validateDoc } from "../src/validate.js";

const fields = {
  amount: t.numeric({ precision: 30, scale: 2 }).required(),
  label: t.string(),
};

describe("exact decimals", () => {
  test("the builder and value factory preserve decimal text", () => {
    assert.deepEqual(fields.amount.toFieldDef(), {
      type: "number",
      precision: 30,
      scale: 2,
      required: true,
    });
    assert.equal(decimal("9007199254740993.00"), "9007199254740993.00");
    for (const value of ["", "+1", "01", ".1", "1.", "NaN"]) {
      assert.throws(() => decimal(value));
    }
    assert.throws(() => decimal(`1e${"0".repeat(4096)}1`));
  });

  test("encrypted numeric builders retain exact decimal facets", () => {
    assert.deepEqual(
      t.encrypted({ of: t.numeric({ precision: 30, scale: 2 }) }).toFieldDef(),
      {
        type: "number",
        precision: 30,
        scale: 2,
        encrypted: true,
        mask: { kind: "full", classification: "pii" },
      },
    );
  });

  test("document validation accepts only exact decimal strings", () => {
    const schema = { amount: fields.amount.toFieldDef() };
    assert.deepEqual(validateDoc({ amount: decimal("1.25") }, schema), { amount: "1.25" });
    assert.throws(() => validateDoc({ amount: 1.25 }, schema));
    assert.throws(() => validateDoc({ amount: "not-a-decimal" }, schema));
  });
});

const amount = decimal("1.25");
const input: RowInput<typeof fields> = { amount };
const filter: Filter<typeof fields> = { amount: { $eq: amount, $in: [amount] } };
const update: UpdateExpression<typeof fields> = { amount: { $inc: amount } };
void [input, filter, update];

// @ts-expect-error exact decimals do not accept JavaScript numbers
const lossyInput: RowInput<typeof fields> = { amount: 1.25 };
// @ts-expect-error exact decimals have equality but no portable ordering
const rangeFilter: Filter<typeof fields> = { amount: { $gt: amount } };
// @ts-expect-error exact decimals have no portable sort order
const sort: SortSpec<typeof fields> = { amount: 1 };
void [lossyInput, rangeFilter, sort];

const typed: Decimal = amount;
void typed;
