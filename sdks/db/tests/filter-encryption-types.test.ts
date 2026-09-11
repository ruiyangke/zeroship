import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import type { Filter } from "@zeroship/db";

const fields = {
  name: t.string().required(),
  emailMasked: t.string().mask({ kind: "email" }),
  ssnRandom: t.encrypted({  }),
  secondSecret: t.encrypted({  }),
  amountSecret: t.encrypted({ wraps: t.number() }),
};

type UserFilter = Filter<typeof fields>;

const okLike: UserFilter = { name: { $like: "A%" } };
const okMaskedLike: UserFilter = { emailMasked: { $ilike: "%@example.com" } };
// @ts-expect-error encrypted fields cannot be filtered
const badEncryptedValue: UserFilter = { secondSecret: "secret" };
// @ts-expect-error encrypted fields cannot be filtered
const badEncryptedEq: UserFilter = { secondSecret: { $eq: "secret" } };
// @ts-expect-error encrypted fields cannot be filtered
const badEncryptedIn: UserFilter = { amountSecret: { $in: [1, 2, 3] } };

// @ts-expect-error randomised-encrypted fields are not filterable
const badRandomValue: UserFilter = { ssnRandom: "secret" };
// @ts-expect-error randomised-encrypted fields are not filterable, even via $eq
const badRandomEq: UserFilter = { ssnRandom: { $eq: "secret" } };
// @ts-expect-error encrypted string fields reject pattern operators
const badDetLike: UserFilter = { secondSecret: { $like: "sk-%" } };
// @ts-expect-error encrypted numeric fields reject range operators
const badDetRange: UserFilter = { amountSecret: { $gt: 10 } };

void badRandomValue;
void badRandomEq;
void badDetLike;
void badDetRange;

test("Filter<S> matches the encrypted-field runtime fence", () => {
  assert.ok(okLike);
  assert.ok(okMaskedLike);
  assert.ok(badEncryptedValue);
  assert.ok(badEncryptedEq);
  assert.ok(badEncryptedIn);
});
