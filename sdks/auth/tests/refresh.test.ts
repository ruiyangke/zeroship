import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createAuthClient } from "../src/client";
import { AuthError, type AuthChangeEvent } from "../src/types";
import { APP_ORIGIN, jsonResponse, makeHarness } from "./harness";

function mintBody(over?: Record<string, unknown>) {
  return {
    user: {
      id: "pws_alice",
      email: "alice@relay.zeroship.ai",
      email_verified: true,
      name: "Alice",
      avatar: null,
      scopes: ["openid", "profile", "email"],
    },
    access_token: "minted.wrapper",
    token_type: "Bearer",
    expires_in: 600,
    expires_at: Math.floor(Date.now() / 1000) + 600,
    ...over,
  };
}

describe("refreshSession / silent renewal (GET /session?mint=1 under navigator.locks)", () => {
  test("refreshSession mints via /session?mint=1 with X-ZS-Auth and emits TOKEN_REFRESHED", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const session = await client.refreshSession();
    assert.equal(session.access_token, "minted.wrapper");
    const req = h.fetch.requests.find((r) => r.url.includes("mint=1"))!;
    assert.equal(req.headers["x-zs-auth"], "1");
    assert.deepEqual(events, ["TOKEN_REFRESHED"]);
  });

  test("concurrent refreshes are serialized under the lock and coalesced into ONE Hydra round-trip", async () => {
    const h = makeHarness();
    let mintCalls = 0;
    h.fetch.on((u) => u.includes("mint=1"), async () => {
      mintCalls++;
      await new Promise((r) => setTimeout(r, 10));
      return jsonResponse(200, mintBody());
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    const [a, b, c] = await Promise.all([
      client.refreshSession(),
      client.refreshSession(),
      client.refreshSession(),
    ]);
    assert.equal(a.access_token, "minted.wrapper");
    assert.equal(b.access_token, "minted.wrapper");
    assert.equal(c.access_token, "minted.wrapper");
    // The in-flight single-flight coalesces concurrent callers → one mint.
    assert.equal(mintCalls, 1, "concurrent refreshes must coalesce to a single mint");
    // The lock was actually acquired (serialization observable).
    assert.ok(h.locks.order.includes(`acquire:zs.refresh.${APP_ORIGIN}`));
  });

  test("getAccessToken returns the cached token within skew, mints when stale", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);

    // No cached session yet → must mint.
    const t1 = await client.getAccessToken();
    assert.equal(t1, "minted.wrapper");
    const after = h.fetch.requests.length;
    // Fresh token now cached well beyond skew → no second mint.
    const t2 = await client.getAccessToken();
    assert.equal(t2, "minted.wrapper");
    assert.equal(h.fetch.requests.length, after, "cached token within skew skips the network");
  });

  test("login_required during refresh clears the breadcrumb and signs out", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(401, { error: "login_required" }));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    await assert.rejects(client.refreshSession(), (e: unknown) => {
      assert.equal((e as AuthError).code, "login_required");
      return true;
    });
    assert.doesNotMatch(h.cookies.get(), /is\.authenticated=true/);
  });

  test("refresh works WITHOUT the Web Locks API (in-process fallback serializes)", async () => {
    const h = makeHarness({ withLocks: false });
    let mintCalls = 0;
    h.fetch.on((u) => u.includes("mint=1"), async () => {
      mintCalls++;
      await new Promise((r) => setTimeout(r, 5));
      return jsonResponse(200, mintBody());
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const [a, b] = await Promise.all([client.refreshSession(), client.refreshSession()]);
    assert.equal(a.access_token, "minted.wrapper");
    assert.equal(b.access_token, "minted.wrapper");
    assert.equal(mintCalls, 1, "coalesced even without navigator.locks");
  });
});

describe("checkSession — breadcrumb-gated rehydration (gateway §4.3)", () => {
  test("FIRST checkSession probes unconditionally even with NO breadcrumb, recovers SIGNED_IN", async () => {
    const h = makeHarness();
    // No breadcrumb cookie set — but a live anchor exists server-side.
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const session = await client.checkSession();
    assert.equal(session?.access_token, "minted.wrapper");
    assert.deepEqual(events, ["SIGNED_IN"]);
    assert.ok(
      h.fetch.requests.some((r) => r.url.includes("mint=1")),
      "the first probe must run regardless of the breadcrumb",
    );
  });

  test("FIRST checkSession SHORT-CIRCUITS on a live in-memory session, no mint", async () => {
    // A caller that already holds a non-expired session (e.g. just signed in,
    // or hydrated from localStorage) must not pay an extra unconditional mint
    // on init — the in-memory session already reflects server state.
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    // Establish a live session via a real refresh, then count network calls.
    await client.refreshSession();
    const afterRefresh = h.fetch.requests.length;
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const s = await client.checkSession();
    assert.equal(s?.access_token, "minted.wrapper");
    assert.equal(
      h.fetch.requests.length,
      afterRefresh,
      "a live cached session must short-circuit the unconditional first probe",
    );
    assert.deepEqual(events, [], "no event re-emitted when serving the cached session");
  });

  test("REPEAT checkSession with no breadcrumb stays anonymous WITHOUT a network call", async () => {
    const h = makeHarness();
    let mintCalls = 0;
    h.fetch.on((u) => u.includes("mint=1"), () => {
      mintCalls++;
      return jsonResponse(401, { error: "login_required" });
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    // First call probes (401 → anonymous, breadcrumb cleared/absent).
    await client.checkSession();
    const afterFirst = mintCalls;
    // Second call: no breadcrumb → must early-return without a network call.
    const s = await client.checkSession();
    assert.equal(s, null);
    assert.equal(mintCalls, afterFirst, "repeat probe must be suppressed when the breadcrumb is absent");
  });

  test("401 login_required on first probe → clean anonymous, no SIGNED_OUT noise, breadcrumb cleared", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(401, { error: "login_required" }));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));
    const s = await client.checkSession();
    assert.equal(s, null);
    assert.deepEqual(events, [], "no SIGNED_OUT when there was never a session");
    assert.doesNotMatch(h.cookies.get(), /is\.authenticated=true/);
  });

  test("503 client_not_provisioned retries under backoff (RECOVERING), then SIGNED_IN, breadcrumb intact", async () => {
    const h = makeHarness();
    // Seed a breadcrumb to prove a 503 does NOT clear it.
    h.cookies.set("zs.myapp.zeroship.ai.is.authenticated=true; Path=/; SameSite=Lax; Max-Age=2592000");
    let call = 0;
    h.fetch.on((u) => u.includes("mint=1"), () => {
      call++;
      if (call === 1) return jsonResponse(503, { error: "client_not_provisioned" });
      return jsonResponse(200, mintBody());
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const session = await client.checkSession();
    assert.equal(session?.access_token, "minted.wrapper");
    assert.equal(call, 2, "503 then 200 — retried once");
    assert.deepEqual(events, ["RECOVERING", "SIGNED_IN"]);
    assert.ok(events.indexOf("SIGNED_OUT") === -1, "no SIGNED_OUT on a 503 recovery");
    assert.match(h.cookies.get(), /is\.authenticated=true/, "503 must NOT clear the breadcrumb");
  });
});
