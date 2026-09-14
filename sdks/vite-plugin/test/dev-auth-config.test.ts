/**
 * `resolveDevAuth` turns the plugin option into the Vite provider's serialized
 * user config and its runtime-shared cookie secret.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveDevAuth } from "../src/dev-auth-config.js";

const SECRET = "deadbeef";
const gen = () => SECRET;

describe("resolveDevAuth", () => {
  test("undefined → default ON (built-in user, sentinel '1', a secret)", () => {
    const r = resolveDevAuth(undefined, gen);
    assert.equal(r.config, "1");
    assert.equal(r.secret, SECRET);
  });

  test("true → default ON", () => {
    const r = resolveDevAuth(true, gen);
    assert.equal(r.config, "1");
    assert.equal(r.secret, SECRET);
  });

  test("false → disabled (no config, no secret)", () => {
    const r = resolveDevAuth(false, gen);
    assert.equal(r.config, null);
    assert.equal(r.secret, null);
  });

  test("bare single-user object → { user }", () => {
    const r = resolveDevAuth({ email: "a@b.c", scopes: ["openid"] }, gen);
    assert.equal(r.secret, SECRET);
    assert.deepEqual(JSON.parse(r.config!), { user: { email: "a@b.c", scopes: ["openid"] } });
  });

  test("{ user } → { user }", () => {
    const r = resolveDevAuth({ user: { id: "pws_x" } }, gen);
    assert.deepEqual(JSON.parse(r.config!), { user: { id: "pws_x" } });
  });

  test("{ users, defaultUserId } → preserved verbatim", () => {
    const r = resolveDevAuth(
      { users: [{ id: "pws_a" }, { id: "pws_b" }], defaultUserId: "pws_b" },
      gen,
    );
    assert.deepEqual(JSON.parse(r.config!), {
      users: [{ id: "pws_a" }, { id: "pws_b" }],
      defaultUserId: "pws_b",
    });
  });

  test("{ users } without defaultUserId → omits the key", () => {
    const r = resolveDevAuth({ users: [{ id: "pws_a" }] }, gen);
    const parsed = JSON.parse(r.config!);
    assert.deepEqual(parsed, { users: [{ id: "pws_a" }] });
    assert.equal("defaultUserId" in parsed, false);
  });

  test("the identity fields are carried through to the dev runtime config verbatim", () => {
    // The dev login form + the runtime's identity injection read these out of
    // ZEROSHIP_DEV_AUTH, so they must survive serialization unchanged. There is
    // deliberately no `password` among them: the dev tier DERIVES it from `id`
    // (`devPasswordFor` in the Vite dev-auth provider), so there is nothing
    // to carry.
    const user = {
      id: "pws_alice000000000000000",
      email: "a@b.c",
      name: "Alice",
      avatar: null,
      scopes: ["openid", "admin"],
    };
    const r = resolveDevAuth({ user }, gen);
    assert.deepEqual(JSON.parse(r.config!), { user });
    assert.equal(r.config!.includes("password"), false);
  });
});
