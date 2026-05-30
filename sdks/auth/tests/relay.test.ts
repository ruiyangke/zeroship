import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveEnv } from "../src/internal/env";
import { listenForRelay } from "../src/internal/relay";
import { AuthError } from "../src/types";
import { APP_ORIGIN, makeHarness } from "./harness";

const RESPONSE_OK = {
  type: "zs:authorization_response",
  response: { code: "auth-code-123", state: "state-abc" },
};

describe("relay listener — postMessage handshake (gateway §4.4)", () => {
  test("resolves on a valid postMessage from the app origin", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    h.window.dispatchMessage({ origin: APP_ORIGIN, data: RESPONSE_OK });
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
    assert.equal(r.state, "state-abc");
  });

  test("rejects a message from the WRONG origin with config_error (not a silent wait)", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    h.window.dispatchMessage({ origin: "https://evil.example", data: RESPONSE_OK });
    await assert.rejects(relay.promise, (e: unknown) => {
      assert.ok(e instanceof AuthError);
      assert.equal((e as AuthError).code, "config_error");
      return true;
    });
  });

  test("ignores a message with the wrong envelope type (no false positive)", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    // A foreign library postMessage — must NOT resolve.
    h.window.dispatchMessage({ origin: APP_ORIGIN, data: { type: "other:thing" } });
    let settled = false;
    relay.promise.then(() => (settled = true)).catch(() => (settled = true));
    await new Promise((r) => setTimeout(r, 5));
    assert.equal(settled, false, "non-matching envelope must not settle the relay");
    relay.dispose();
  });

  test("BroadcastChannel fallback delivers when opener is severed (COOP)", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    // Simulate the popup-callback page posting over the same-origin channel.
    const ch = h.env.broadcastChannel!("zs:auth");
    ch.postMessage(RESPONSE_OK);
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
  });

  test("one-shot localStorage relay (storage event) delivers", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    h.fireStorage("@@zsauth@@::relay::state-abc", JSON.stringify(RESPONSE_OK));
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
  });

  test("error response is carried through the relay", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN);
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: {
        type: "zs:authorization_response",
        response: { error: "login_required", error_description: "no session", state: "s" },
      },
    });
    const r = await relay.promise;
    assert.equal(r.error, "login_required");
  });
});
