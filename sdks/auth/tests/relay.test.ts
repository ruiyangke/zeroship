import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { resolveEnv } from "../src/internal/env";
import { listenForRelay } from "../src/internal/relay";
import { AuthError } from "../src/types";
import { APP_ORIGIN, makeHarness } from "./harness";

const STATE = "state-abc";
const RESPONSE_OK = {
  type: "zs:authorization_response",
  response: { code: "auth-code-123", state: STATE },
};

describe("relay listener — postMessage handshake (gateway §4.4)", () => {
  test("resolves on a valid postMessage from the app origin", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN, STATE);
    h.window.dispatchMessage({ origin: APP_ORIGIN, data: RESPONSE_OK });
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
    assert.equal(r.state, STATE);
  });

  test("rejects a message from the WRONG origin with config_error (not a silent wait)", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN, STATE);
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
    const relay = listenForRelay(env, APP_ORIGIN, STATE);
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
    const relay = listenForRelay(env, APP_ORIGIN, STATE);
    // Simulate the popup-callback page posting over the same-origin channel.
    const ch = h.env.broadcastChannel!("zs:auth");
    ch.postMessage(RESPONSE_OK);
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
  });

  test("one-shot localStorage relay (storage event) delivers", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN, STATE);
    h.fireStorage(`@@zsauth@@::relay::${STATE}`, JSON.stringify(RESPONSE_OK));
    const r = await relay.promise;
    assert.equal(r.code, "auth-code-123");
  });

  test("error response is carried through the relay", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN, "s");
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

describe("relay listener — state filtering (MAJOR fix: no cross-flow delivery)", () => {
  test("IGNORES (keeps waiting) an envelope whose state does not match", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relay = listenForRelay(env, APP_ORIGIN, "my-state");
    // A well-formed response from a CONCURRENT flow (right origin, wrong state).
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: {
        type: "zs:authorization_response",
        response: { code: "other-flow-code", state: "other-state" },
      },
    });
    let settled = false;
    relay.promise.then(() => (settled = true)).catch(() => (settled = true));
    await new Promise((r) => setTimeout(r, 5));
    assert.equal(settled, false, "a mismatched-state envelope must NOT settle (keeps waiting)");

    // The flow's OWN response (matching state) settles it with ITS code.
    h.window.dispatchMessage({
      origin: APP_ORIGIN,
      data: {
        type: "zs:authorization_response",
        response: { code: "my-code", state: "my-state" },
      },
    });
    const r = await relay.promise;
    assert.equal(r.code, "my-code");
    assert.equal(r.state, "my-state");
  });

  test("two concurrent flows each resolve with their OWN code (BroadcastChannel)", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    // Two independent flows on the SAME origin, sharing the BroadcastChannel.
    const relayA = listenForRelay(env, APP_ORIGIN, "state-A");
    const relayB = listenForRelay(env, APP_ORIGIN, "state-B");

    const ch = h.env.broadcastChannel!("zs:auth");
    // Broadcast BOTH responses (origin-wide — each flow sees both).
    ch.postMessage({
      type: "zs:authorization_response",
      response: { code: "code-B", state: "state-B" },
    });
    ch.postMessage({
      type: "zs:authorization_response",
      response: { code: "code-A", state: "state-A" },
    });

    const [a, b] = await Promise.all([relayA.promise, relayB.promise]);
    assert.equal(a.code, "code-A", "flow A must get A's code, not B's");
    assert.equal(a.state, "state-A");
    assert.equal(b.code, "code-B", "flow B must get B's code, not A's");
    assert.equal(b.state, "state-B");
  });

  test("two concurrent flows via origin-wide storage events do not cross-deliver", async () => {
    const h = makeHarness();
    const env = resolveEnv(h.env);
    const relayA = listenForRelay(env, APP_ORIGIN, "st-A");
    const relayB = listenForRelay(env, APP_ORIGIN, "st-B");

    // A single origin-wide storage event is delivered to EVERY listener.
    h.fireStorage(
      "@@zsauth@@::relay::st-B",
      JSON.stringify({
        type: "zs:authorization_response",
        response: { code: "sc-B", state: "st-B" },
      }),
    );
    h.fireStorage(
      "@@zsauth@@::relay::st-A",
      JSON.stringify({
        type: "zs:authorization_response",
        response: { code: "sc-A", state: "st-A" },
      }),
    );

    const [a, b] = await Promise.all([relayA.promise, relayB.promise]);
    assert.equal(a.code, "sc-A");
    assert.equal(b.code, "sc-B");
  });
});
