/**
 * `resolveDevAuthEnv` — the plugin-side resolver that turns the `devAuth`
 * option into the `ZEROSHIP_DEV_AUTH` / `ZEROSHIP_DEV_AUTH_SECRET` env pair the
 * spawned dev runtime reads. Asserts the default-ON behaviour, the disable
 * path, and the three user-config shapes. The serialized JSON is exactly what
 * `@zeroship/bootstrap`'s `parseDevAuthConfig` consumes.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveDevAuthEnv } from "../src/dev-auth-config.js";

const SECRET = "deadbeef";
const gen = () => SECRET;

describe("resolveDevAuthEnv", () => {
  test("undefined → default ON (built-in user, sentinel '1', a secret)", () => {
    const r = resolveDevAuthEnv(undefined, gen);
    assert.equal(r.config, "1");
    assert.equal(r.secret, SECRET);
  });

  test("true → default ON", () => {
    const r = resolveDevAuthEnv(true, gen);
    assert.equal(r.config, "1");
    assert.equal(r.secret, SECRET);
  });

  test("false → disabled (no config, no secret)", () => {
    const r = resolveDevAuthEnv(false, gen);
    assert.equal(r.config, null);
    assert.equal(r.secret, null);
  });

  test("bare single-user object → { user }", () => {
    const r = resolveDevAuthEnv({ email: "a@b.c", scopes: ["openid"] }, gen);
    assert.equal(r.secret, SECRET);
    assert.deepEqual(JSON.parse(r.config!), { user: { email: "a@b.c", scopes: ["openid"] } });
  });

  test("{ user } → { user }", () => {
    const r = resolveDevAuthEnv({ user: { id: "pws_x" } }, gen);
    assert.deepEqual(JSON.parse(r.config!), { user: { id: "pws_x" } });
  });

  test("{ users, defaultUserId } → preserved verbatim", () => {
    const r = resolveDevAuthEnv(
      { users: [{ id: "pws_a" }, { id: "pws_b" }], defaultUserId: "pws_b" },
      gen,
    );
    assert.deepEqual(JSON.parse(r.config!), {
      users: [{ id: "pws_a" }, { id: "pws_b" }],
      defaultUserId: "pws_b",
    });
  });

  test("{ users } without defaultUserId → omits the key", () => {
    const r = resolveDevAuthEnv({ users: [{ id: "pws_a" }] }, gen);
    const parsed = JSON.parse(r.config!);
    assert.deepEqual(parsed, { users: [{ id: "pws_a" }] });
    assert.equal("defaultUserId" in parsed, false);
  });
});
