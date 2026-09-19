/**
 * The `.default` / `.clientDefault` split.
 *
 * `.default(value)` is the DATABASE default (scalar/container, lowered to SQL
 * `DEFAULT`); `.clientDefault(fn)` is the SDK-evaluated default run at insert.
 * A factory function in the `.default` position is refused, and a client
 * default never lands in the `default` slot the descriptor/fold lowers.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import { validateDoc } from "../../../crates/zeroship-data-v8/js/runtime/validate.js";
import { fieldsOf } from "./_install-helper.js";

type ErrorWithCode = Error & { code?: string };

describe(".default / .clientDefault split", () => {
  test("a factory function passed to .default() is refused", () => {
    assert.throws(
      () => (t.string() as unknown as { default(v: unknown): unknown }).default(() => "x"),
      (e: unknown) => (e as ErrorWithCode).code === "DEFAULT_FACTORY_REQUIRES_CLIENT_DEFAULT",
    );
  });

  test("a scalar .default() stays the database default", () => {
    const def = t.string().default("user").toFieldDef();
    assert.equal(def.default, "user");
    assert.equal(def.clientDefault, undefined);
  });

  test(".clientDefault(fn) carries a client default and does not lower to SQL DEFAULT", () => {
    const fn = () => "generated";
    const def = t.string().clientDefault(fn).toFieldDef();
    assert.equal(def.default, undefined);
    assert.equal(def.clientDefault, fn);
  });

  test(".clientDefault(nonFunction) is refused", () => {
    assert.throws(
      () => (t.string() as unknown as { clientDefault(v: unknown): unknown }).clientDefault("nope"),
      (e: unknown) => (e as ErrorWithCode).code === "CLIENT_DEFAULT_REQUIRES_FUNCTION",
    );
  });

  test("validateDoc evaluates a client default at insert", () => {
    const schema = fieldsOf({ token: t.string().clientDefault(() => "tok") });
    assert.equal(validateDoc({}, schema).token, "tok");
  });

  test("a required field with a client default is not missing", () => {
    const schema = fieldsOf({
      token: t.string().required().clientDefault(() => "tok"),
    });
    assert.equal(validateDoc({}, schema).token, "tok");
  });

  test("a provided value wins over the client default", () => {
    const schema = fieldsOf({ token: t.string().clientDefault(() => "tok") });
    assert.equal(validateDoc({ token: "given" }, schema).token, "given");
  });

  test("a nested required field with a client default is not rejected", () => {
    const schema = fieldsOf({
      profile: t.object({ token: t.string().required().clientDefault(() => "tok") }),
    });
    assert.doesNotThrow(() => validateDoc({ profile: {} }, schema));
  });
});
