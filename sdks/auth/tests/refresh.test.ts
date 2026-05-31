import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createAuthClient } from "../src/client";
import { AuthError, type AuthChangeEvent } from "../src/types";
import { APP_ORIGIN, jsonResponse, makeHarness } from "./harness";

// BFF model — `GET /session?mint=1` returns an identity projection ONLY
// (`{user, expires_at}`); the re-signed `__Host-zs_app_session` cookie is the
// credential, never a token in the body. `refreshSession` re-mints that cookie
// + refreshes the identity snapshot — there is NO token to refresh.
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
    expires_at: Math.floor(Date.now() / 1000) + 600,
    ...over,
  };
}

describe("refreshSession — cookie/identity re-mint (GET /session?mint=1)", () => {
  test("refreshSession re-mints the cookie via /session?mint=1 with X-ZS-Auth and emits SESSION_REFRESHED", async () => {
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const session = await client.refreshSession();
    // Identity-only — no token field on the result.
    assert.equal(
      "access_token" in (session as unknown as Record<string, unknown>),
      false,
      "BFF model: the Session carries no token",
    );
    assert.equal(session.user.id, "pws_alice");
    // The granted scopes flow through the /session?mint=1 response onto
    // Session.scopes (the gateway includes them in the user projection).
    assert.deepEqual(session.scopes, ["openid", "profile", "email"]);
    const req = h.fetch.requests.find((r) => r.url.includes("mint=1"))!;
    assert.equal(req.headers["x-zs-auth"], "1");
    assert.deepEqual(events, ["SESSION_REFRESHED"]);
  });

  test("concurrent refreshes coalesce into ONE cookie re-mint (in-tab single-flight)", async () => {
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
    assert.equal(a.user.id, "pws_alice");
    assert.equal(b.user.id, "pws_alice");
    assert.equal(c.user.id, "pws_alice");
    // The in-flight single-flight coalesces concurrent callers → one re-mint.
    // (Cross-tab coalescing is the gateway's per-anchor mint single-flight.)
    assert.equal(mintCalls, 1, "concurrent refreshes must coalesce to a single re-mint");
  });

  test("a second refresh AFTER the first settles re-mints again (no stale coalescing)", async () => {
    const h = makeHarness();
    let mintCalls = 0;
    h.fetch.on((u) => u.includes("mint=1"), () => {
      mintCalls++;
      return jsonResponse(200, mintBody());
    });
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    await client.refreshSession();
    await client.refreshSession();
    assert.equal(mintCalls, 2, "sequential refreshes each re-mint the cookie");
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

  test("403 scope_required maps to the scope_required AuthErrorCode", async () => {
    // Gateway scope gate (auth-sdk Slice 3c, §5.3): the 403 JSON body is
    // `{"error":"scope_required","scope":"…"}`. The SDK mapError() known-map
    // must surface it as `scope_required` (NOT fall through to server_error),
    // so callers can prompt for the missing grant.
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () =>
      jsonResponse(403, { error: "scope_required", scope: "read:billing" }),
    );
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    await assert.rejects(client.refreshSession(), (e: unknown) => {
      assert.equal((e as AuthError).code, "scope_required");
      return true;
    });
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
    assert.equal(
      "access_token" in (session as unknown as Record<string, unknown>),
      false,
      "BFF model: no token on the recovered session",
    );
    assert.equal(session?.user.id, "pws_alice");
    assert.deepEqual(events, ["SIGNED_IN"]);
    assert.ok(
      h.fetch.requests.some((r) => r.url.includes("mint=1")),
      "the first probe must run regardless of the breadcrumb",
    );
  });

  test("FIRST checkSession SHORT-CIRCUITS on a live in-memory session, no mint", async () => {
    // A caller that already holds a non-expired identity snapshot (e.g. just
    // signed in) must not pay an extra unconditional mint on init — the
    // in-memory snapshot already reflects server state.
    const h = makeHarness();
    h.fetch.on((u) => u.includes("mint=1"), () => jsonResponse(200, mintBody()));
    const client = createAuthClient({ appOrigin: APP_ORIGIN }, h.env);
    // Establish a live session via a real refresh, then count network calls.
    await client.refreshSession();
    const afterRefresh = h.fetch.requests.length;
    const events: AuthChangeEvent[] = [];
    client.onAuthStateChange((e) => events.push(e));

    const s = await client.checkSession();
    assert.equal(s?.user.id, "pws_alice");
    assert.equal(
      h.fetch.requests.length,
      afterRefresh,
      "a live in-memory snapshot must short-circuit the unconditional first probe",
    );
    assert.deepEqual(events, [], "no event re-emitted when serving the cached snapshot");
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
    assert.equal(session?.user.id, "pws_alice");
    assert.equal(call, 2, "503 then 200 — retried once");
    assert.deepEqual(events, ["RECOVERING", "SIGNED_IN"]);
    assert.ok(events.indexOf("SIGNED_OUT") === -1, "no SIGNED_OUT on a 503 recovery");
    assert.match(h.cookies.get(), /is\.authenticated=true/, "503 must NOT clear the breadcrumb");
  });
});
