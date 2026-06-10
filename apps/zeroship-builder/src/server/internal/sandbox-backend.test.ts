"use server";
// SEC-6 regression: the sandbox owner for every file/exec/preview/env
// operation must be derived from the request's STABLE platform identity
// — the gateway forwards the per-app PAIRWISE subject (`pws_…`, never a
// `usr_…` typed-id) in `ZeroShip-User` — and the dev single-user
// fallback must only ever engage behind the platform's POSITIVE dev
// signal (`ZEROSHIP_DEV=1`, injected by the Vite dev runtime and
// structurally absent from the deployed V8 worker).
//
// Pre-fix failure mode: `resolveSandboxUserId` required a `usr_` typed
// id and otherwise fell back to the single constant
// `DEV_SANDBOX_USER_ID` whenever `process.env.NODE_ENV !== "production"`
// — and the deployed runtime never injects NODE_ENV. Every
// authenticated production user (all `pws_…`) therefore collapsed onto
// ONE sandbox owner: shared container, shared workspace, shared `.env`,
// shared shell. (And had NODE_ENV=production ever been set, the
// usr_-only assertion would instead have thrown for every real user.)
//
// The sandbox controller hard-requires `usr_<base62-uuidv7>` owner ids
// (crates/sandbox/src/handlers.rs `is_typed_id(user_id, "usr")`), and a
// raw pairwise subject is 20 base62 chars (not a 22-char typed-id) — so
// the per-user owner must be a DETERMINISTIC `usr_`-shaped projection
// of the pairwise subject, not the raw subject string.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  currentUser: vi.fn<() => unknown>(),
}));

vi.mock("zeroship", () => ({
  currentUser: mocks.currentUser,
}));

import { isTypedId } from "@zeroship/server/typed-id";
import {
  DEV_SANDBOX_USER_ID,
  resolveSandboxUserId,
} from "./sandbox-backend";

// Real gateway shape: `pws_` + base62(HMAC)[:20] (crates/core auth
// derive_pairwise). 20 chars — deliberately NOT a valid 22-char
// typed-id.
const PWS_ALICE = "pws_4kT9mQ2tYx0Zr1WvNc9A";
const PWS_BOB = "pws_Zz81LhJq7eRw3PbKd5Xy";

// The deployed V8 runtime injects neither NODE_ENV nor ZEROSHIP_DEV;
// vitest sets NODE_ENV=test and the host shell may carry either — clear
// them so each test starts from the deployed-runtime environment and
// opts into signals explicitly.
const ENV_KEYS = ["NODE_ENV", "ZEROSHIP_DEV"] as const;
let savedEnv: Record<string, string | undefined>;

beforeEach(() => {
  savedEnv = {};
  for (const key of ENV_KEYS) {
    savedEnv[key] = process.env[key];
    delete process.env[key];
  }
  mocks.currentUser.mockReset();
});

afterEach(() => {
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
});

describe("SEC-6: resolveSandboxUserId", () => {
  it("derives a stable PER-USER owner from the gateway pairwise subject (no dev flag set)", () => {
    mocks.currentUser.mockReturnValue({ id: PWS_ALICE });
    const alice = resolveSandboxUserId();

    // The core regression: an authenticated pws_ user must NOT collapse
    // onto the shared dev sandbox owner.
    expect(alice).not.toBe(DEV_SANDBOX_USER_ID);
    // Controller-compatible shape (crates/sandbox requires usr_ typed-ids).
    expect(isTypedId(alice, "usr")).toBe(true);
    // Stable: the same subject always owns the same sandbox.
    expect(resolveSandboxUserId()).toBe(alice);

    // Per-user: a different pairwise subject gets a DIFFERENT owner —
    // no shared container/workspace/.env/shell.
    mocks.currentUser.mockReturnValue({ id: PWS_BOB });
    const bob = resolveSandboxUserId();
    expect(bob).not.toBe(DEV_SANDBOX_USER_ID);
    expect(bob).not.toBe(alice);
    expect(isTypedId(bob, "usr")).toBe(true);
  });

  it("fails CLOSED when unauthenticated and no positive dev signal is set", () => {
    // currentUser() throws outside an authenticated request context.
    mocks.currentUser.mockImplementation(() => {
      throw new Error("no request context");
    });
    expect(() => resolveSandboxUserId()).toThrow(/not authenticated/);
  });

  it("does not throw for pairwise subjects even when NODE_ENV=production is set", () => {
    // The pre-fix code had two broken arms; this exercises the second
    // (NODE_ENV=production → usr_-only assertion threw for every real
    // pws_ user, a full outage instead of a collapse).
    process.env.NODE_ENV = "production";
    mocks.currentUser.mockReturnValue({ id: PWS_ALICE });
    const owner = resolveSandboxUserId();
    expect(owner).not.toBe(DEV_SANDBOX_USER_ID);
    expect(isTypedId(owner, "usr")).toBe(true);
  });

  it("uses the single dev owner only behind the explicit ZEROSHIP_DEV=1 signal when unauthenticated", () => {
    process.env.ZEROSHIP_DEV = "1";
    mocks.currentUser.mockImplementation(() => {
      throw new Error("no request context");
    });
    expect(resolveSandboxUserId()).toBe(DEV_SANDBOX_USER_ID);
  });

  it("authenticated identity wins over the dev fallback even in dev", () => {
    process.env.ZEROSHIP_DEV = "1";
    mocks.currentUser.mockReturnValue({ id: PWS_ALICE });
    const owner = resolveSandboxUserId();
    expect(owner).not.toBe(DEV_SANDBOX_USER_ID);
    expect(isTypedId(owner, "usr")).toBe(true);
  });

  it("explicit (test escape-hatch) ids must already be usr_ typed-ids", () => {
    const explicit = "usr_0000000000000000000001";
    expect(resolveSandboxUserId(explicit)).toBe(explicit);
    expect(() => resolveSandboxUserId("prj_not_a_user")).toThrow(/usr_ typed-id/);
  });
});
