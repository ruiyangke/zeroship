import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import type { Filter } from "../src/index.js";

const fields = {
  name: t.string().required(),
  enabled: t.boolean(),
  payload: t.json(),
  embedding: t.vector(2),
  location: t.geoPoint(),
  emailMasked: t.string().mask({ kind: "email" }),
  ssnRandom: t.encrypted({  }),
  secondSecret: t.encrypted({  }),
  amountSecret: t.encrypted({ of: t.number() }),
};

type UserFilter = Filter<typeof fields>;

const okLike: UserFilter = { name: { $like: "A%" } };
const okMaskedLike: UserFilter = { emailMasked: { $ilike: "%@example.com" } };
const okJsonEquality: UserFilter = { payload: { $eq: { key: true } } };
// @ts-expect-error booleans have equality operators but no ordering operators
const badBooleanRange: UserFilter = { enabled: { $gt: false } };
// @ts-expect-error JSON values have equality operators but no ordering operators
const badJsonRange: UserFilter = { payload: { $lt: { key: true } } };
// @ts-expect-error vectors use search rather than ordinary comparison operators
const badVectorEquality: UserFilter = { embedding: { $eq: [1, 2] } };
// @ts-expect-error geographic points use near rather than ordinary comparison operators
const badGeoEquality: UserFilter = { location: { $eq: { lat: 1, lng: 2 } } };
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
void badBooleanRange;
void badJsonRange;
void badVectorEquality;
void badGeoEquality;

test("Filter<S> matches the encrypted-field runtime fence", () => {
  assert.ok(okLike);
  assert.ok(okMaskedLike);
  assert.ok(okJsonEquality);
  assert.ok(badEncryptedValue);
  assert.ok(badEncryptedEq);
  assert.ok(badEncryptedIn);
});
