/**
 * DEV-vs-DEPLOYED SEAM - the machine `code` that `auth.requireUser()` throws
 * must be the exact token the RPC client's `onAuthExpired` hook branches on
 * (`sdks/rpc/src/transport.ts`: `err.code === "UNAUTHENTICATED"`, and the two
 * sibling guards in `transport.ts` / `batch.ts`).
 *
 * Why the mismatch is not cosmetic:
 *
 *   DEPLOYED, an anonymous caller never reaches the worker - the gateway's
 *   auth gate answers 401 with `{"code":"UNAUTHENTICATED"}`
 *   (`crates/gateway/src/router/dispatch.rs`, `unauthenticated_response`), so
 *   `onAuthExpired` fires and the app re-authenticates.
 *
 *   IN DEV there is no gateway. `requireUser()` itself is the 401 source. Its
 *   `code` is copied verbatim into the error body by the bootstrap fetch
 *   handler (`errorBodyFromThrown`) and lifted verbatim again by the client
 *   (`parseErrorResponse` takes `parsed.code` when present and only falls back
 *   to the status-derived "UNAUTHENTICATED" when the body carries NO code).
 *   So a divergent producer code does not merely fail to match - it OVERRIDES
 *   the otherwise-correct status-derived one. An app that re-authenticates
 *   from `onAuthExpired` works deployed and silently does not in dev.
 *
 * This test drives the three real parties end to end - the real
 * `auth.requireUser()` (this package), the real `__zsDispatch` + fetch handler
 * (`@zeroship/bootstrap`), and the real `@zeroship/rpc` client transport.
 * Nothing about the code string is hard-coded on the server side: the handler
 * calls `requireUser()` and whatever it throws is what travels the wire.
 *
 * WHAT THIS FILE DOES NOT COVER: the batch link's two `onAuthExpired` guards
 * (`sdks/rpc/src/batch.ts`). `createRpcClient` never routes through the batch
 * link, and no server in this repo implements `POST /__zeroship/v1/_batch`, so
 * there is no faithful seam to drive there.
 */

import { test, describe, beforeEach } from "node:test";
import assert from "node:assert/strict";

// Side-effect import: installs the real `globalThis.__zsDispatch`.
import "@zeroship/bootstrap/dispatcher";
import { createFetchHandler } from "@zeroship/bootstrap/fetch-handler";
import { createRpcClient, isRpcError, type RpcError } from "@zeroship/rpc/client";
import { env } from "zeroship";

import { auth } from "../src/server.js";

const BASE_URL = "https://app.test";

/**
 * The server half: the real bootstrap fetch handler over an RPC table with
 * two procedures that differ in exactly ONE variable - the error code.
 *
 *   `guarded` - calls the real `auth.requireUser()`. With no auth plugin
 *     registered the SDK's own throw is the 401 source, exactly as in a dev
 *     run with no gateway in front.
 *
 *   `notAuth` - the one-variable control. Same `status: 401`, same thrown
 *     shape, same envelope, DIFFERENT code. If `onAuthExpired` fired for this
 *     one too, a green `guarded` would prove nothing: it would mean the hook
 *     fires on any 401 rather than on the auth code specifically.
 */
const handler = createFetchHandler(async () => ({
  userDefault: {},
  fetch: undefined,
  rpc: {
    guarded() {
      return auth.requireUser();
    },
    notAuth() {
      throw Object.assign(new Error("Authentication required"), {
        status: 401,
        code: "INVALID_ARGUMENT",
      });
    },
  },
}));

/** Route the client's fetch straight into the handler (no socket needed). */
const fetchIntoHandler: typeof globalThis.fetch = async (input, init) =>
  handler(new Request(input as RequestInfo, init), {}, {});

interface CallResult {
  authExpiredCalls: number;
  error: RpcError | undefined;
}

async function callProcedure(id: string): Promise<CallResult> {
  let authExpiredCalls = 0;
  const rpc = createRpcClient({
    baseUrl: BASE_URL,
    fetch: fetchIntoHandler,
    onAuthExpired: () => {
      authExpiredCalls += 1;
    },
  });
  let error: RpcError | undefined;
  try {
    await rpc.mutation(id)(null);
  } catch (e) {
    assert.ok(isRpcError(e), `expected an RpcError from ${id}, got ${String(e)}`);
    error = e as RpcError;
  }
  return { authExpiredCalls, error };
}

describe("onAuthExpired seam - requireUser-sourced 401 (dev, no gateway)", () => {
  // No auth plugin registered: `auth.requireUser()` takes its own throw path,
  // which is the dev-time 401 source this seam is about.
  beforeEach(() => {
    delete (env as Record<string, unknown>).auth;
  });

  test("fires onAuthExpired for a requireUser-sourced 401", async () => {
    const { authExpiredCalls, error } = await callProcedure("guarded");

    assert.ok(error, "the guarded procedure must reject");
    assert.equal(error!.status, 401, "requireUser must surface as HTTP 401");
    assert.equal(
      authExpiredCalls,
      1,
      "onAuthExpired must fire once for a requireUser-sourced 401 - the hook " +
        "keys on the wire code, so a producer code the client does not " +
        "recognise silently disables re-authentication in dev",
    );
    assert.equal(
      error!.code,
      "UNAUTHENTICATED",
      "the code requireUser throws must be the canonical wire token, not a " +
        "private spelling that overrides the status-derived one",
    );
  });

  // ONE-VARIABLE CONTROL. Identical to the case above except for the code
  // string: same 401 status, same throw shape, same envelope, same transport.
  // It separates "the producer emits the right code" from "the hook fires on
  // every failure" / "the hook fires on every 401".
  test("does NOT fire onAuthExpired for a non-auth error at the same 401 status", async () => {
    const { authExpiredCalls, error } = await callProcedure("notAuth");

    assert.ok(error, "the control procedure must reject");
    assert.equal(error!.status, 401, "control must hold the status variable fixed");
    assert.equal(error!.code, "INVALID_ARGUMENT", "control must differ only in the code");
    assert.equal(
      authExpiredCalls,
      0,
      "onAuthExpired must key on the auth code, not on the 401 status or on " +
        "failure in general",
    );
  });
});
