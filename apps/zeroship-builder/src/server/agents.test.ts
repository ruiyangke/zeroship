"use server";
// SEC-10 regression: the builder console is ONE shared zeroship app —
// every creator shares its single `@zeroship/kv` namespace. The
// issue-tracker and quality-scorecard procedures take a client-supplied
// `appId` (an arbitrary string), so keying the shared KV purely on
// `appId` is a cross-tenant IDOR: creator B could read creator A's
// issue list and Critic scorecard (and seed-write into A's slot) just
// by guessing/replaying A's appId. Sibling projects.ts already scopes
// its registry per creator (`projects:${currentCreatorId()}`); these
// tests pin the same isolation for issues + quality.
//
// The tests drive the REAL procedures (`listIssues`,
// `getQualityScores`) and the REAL server-internal writer
// (`setQualityFromCritic` — what the chat middleware fires after every
// Critic round), mocking only the unavoidable seams: `currentUser()`
// (gateway identity) and the `@zeroship/kv` client (in-memory map).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const kvStore = new Map<string, unknown>();

const mocks = vi.hoisted(() => ({
  currentUser: vi.fn<() => unknown>(),
}));

vi.mock("zeroship", () => ({
  currentUser: mocks.currentUser,
}));

vi.mock("@zeroship/kv", () => ({
  kv: {
    get: vi.fn(async (key: string) => ({ data: kvStore.get(key) ?? null, error: null })),
    set: vi.fn(async (key: string, value: unknown) => {
      kvStore.set(key, value);
      return { error: null };
    }),
    delete: vi.fn(async (key: string) => {
      kvStore.delete(key);
      return { error: null };
    }),
  },
}));

import { getQualityScores, listIssues } from "./agents";
import { setQualityFromCritic } from "./internal/agent-writes";

// Real gateway shape: per-app pairwise subjects (`pws_` + 20 base62).
const CREATOR_A = "pws_4kT9mQ2tYx0Zr1WvNc9A";
const CREATOR_B = "pws_Zz81LhJq7eRw3PbKd5Xy";

// Creator A's project id — the value an attacker (creator B) replays.
const APP_ID = "prj_alice_secret_project";

function actAs(creatorId: string): void {
  mocks.currentUser.mockReturnValue({ id: creatorId });
}

beforeEach(() => {
  kvStore.clear();
  mocks.currentUser.mockReset();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("SEC-10: issue tracker is creator-isolated", () => {
  it("creator B cannot read creator A's issue list via A's appId", async () => {
    // A opens their project — seeds + persists A's issue list.
    actAs(CREATOR_A);
    const aliceFirst = await listIssues({ appId: APP_ID });
    const aliceIds = aliceFirst.issues.map((i) => i.id).sort();
    expect(aliceIds.length).toBeGreaterThan(0);

    // B replays A's appId. B must NOT receive A's stored list — the
    // read resolves in B's own scope (a fresh seed with different ids).
    actAs(CREATOR_B);
    const bob = await listIssues({ appId: APP_ID });
    const bobIds = bob.issues.map((i) => i.id).sort();
    expect(bobIds).not.toEqual(aliceIds);

    // …and B's read/seed must not have clobbered A's slot.
    actAs(CREATOR_A);
    const aliceAgain = await listIssues({ appId: APP_ID });
    expect(aliceAgain.issues.map((i) => i.id).sort()).toEqual(aliceIds);
  });

  it("same creator keeps a stable issue list across calls", async () => {
    actAs(CREATOR_A);
    const first = await listIssues({ appId: APP_ID });
    const second = await listIssues({ appId: APP_ID });
    expect(second.issues.map((i) => i.id).sort()).toEqual(
      first.issues.map((i) => i.id).sort(),
    );
  });
});

describe("SEC-10: quality scorecard is creator-isolated", () => {
  it("creator B cannot read creator A's Critic scorecard via A's appId", async () => {
    // A's chat middleware persists a Critic round with a distinctive
    // critical finding (real writer path).
    actAs(CREATOR_A);
    await setQualityFromCritic(APP_ID, [
      {
        dimension: "security",
        severity: "critical",
        note: "A-PRIVATE: hardcoded admin token in src/auth.ts",
      },
    ]);

    // A reads their own scorecard back: the critical drags the grade
    // down and the rationale carries A's private note.
    const alice = await getQualityScores({ appId: APP_ID });
    expect(alice.last_run_at).not.toBeNull();
    expect(JSON.stringify(alice.dimensions)).toContain("A-PRIVATE");

    // B replays A's appId: must NOT see A's scorecard — only the
    // never-scored default snapshot, with none of A's rationale text.
    actAs(CREATOR_B);
    const bob = await getQualityScores({ appId: APP_ID });
    expect(bob.last_run_at).toBeNull();
    expect(JSON.stringify(bob.dimensions)).not.toContain("A-PRIVATE");
  });

  it("same creator round-trips their own scorecard", async () => {
    actAs(CREATOR_A);
    await setQualityFromCritic(APP_ID, [
      { dimension: "performance", severity: "medium", note: "slow LCP on /home" },
    ]);
    const scores = await getQualityScores({ appId: APP_ID });
    expect(scores.last_run_at).not.toBeNull();
    expect(JSON.stringify(scores.dimensions)).toContain("slow LCP on /home");
  });
});
