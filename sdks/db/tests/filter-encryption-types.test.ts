import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import type { Filter } from "@zeroship/db";

const fields = {
  name: t.string().required(),
  emailMasked: t.string().mask({ kind: "email" }),
  ssnRandom: t.encrypted({ mode: "randomised" }),
  ssnDet: t.encrypted({ mode: "deterministic" }),
  amountDet: t.encrypted({ mode: "deterministic", wraps: t.number() }),
};

type UserFilter = Filter<typeof fields>;

const okLike: UserFilter = { name: { $like: "A%" } };
const okMaskedLike: UserFilter = { emailMasked: { $ilike: "%@example.com" } };
const okSearch: UserFilter = { name: { $search: "rust async" } };
const okDetValue: UserFilter = { ssnDet: "secret" };
const okDetEq: UserFilter = { ssnDet: { $eq: "secret" } };
const okDetIn: UserFilter = { amountDet: { $in: [1, 2, 3] } };

// @ts-expect-error randomised-encrypted fields are not filterable
const badRandomValue: UserFilter = { ssnRandom: "secret" };
// @ts-expect-error randomised-encrypted fields are not filterable, even via $eq
const badRandomEq: UserFilter = { ssnRandom: { $eq: "secret" } };
// @ts-expect-error deterministic-encrypted string fields reject pattern operators
const badDetLike: UserFilter = { ssnDet: { $like: "sk-%" } };
// @ts-expect-error deterministic-encrypted numeric fields reject range operators
const badDetRange: UserFilter = { amountDet: { $gt: 10 } };

void badRandomValue;
void badRandomEq;
void badDetLike;
void badDetRange;

test("Filter<S> matches the encrypted-field runtime fence", () => {
  assert.ok(okLike);
  assert.ok(okMaskedLike);
  assert.ok(okSearch);
  assert.ok(okDetValue);
  assert.ok(okDetEq);
  assert.ok(okDetIn);
});
