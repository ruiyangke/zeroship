"use server";
// Agent context server functions.
//
// The builder chat, PM worker, and mention dropdown need a small,
// project-scoped read model for issues and quality scorecards. This
// module exposes that read model as narrow RPC procedures.
//
// Storage shape: KV-backed via `internal/persist.ts`. The native `@zeroship/kv`
// primitive lives in the V8 worker process, which the vite-plugin keeps
// running across HMR module re-evaluations — so writes survive "save
// the file → dev refreshes the bundle". They DO vanish on a hard worker
// restart, which is consistent with "dev only, single worker" semantics
// of the in-memory KV backend. Production will replace these with real
// backing tables.
//
// Wire convention: every export takes ONE object input (per the
// builder's single-input RPC wire — see `apps/zeroship-builder/src/
// server/sandbox.ts` header for the long form). That keeps the
// `vite-plugin` `args[0]`-only forwarding honest.

import { query, mutation } from "@zeroship/rpc/server";
import { z } from "zod";
import { persistGet, persistSet } from "./internal/persist";

// ─── issue store ────────────────────────────────────────────────

export type IssueStatus = "open" | "in_progress" | "done";
export type IssueSource = "you" | "pm" | "sre" | "builder";

const appIdSchema = z.string().min(1).max(256);
const appIdInputSchema = z.object({ appId: appIdSchema }).strict();

export interface Issue {
  id: string;
  title: string;
  description: string;
  status: IssueStatus;
  source: IssueSource;
  /** Display name of the assignee, or null if unassigned. */
  assignee: string | null;
  /** ISO-8601 timestamps. */
  created_at: string;
  updated_at: string;
  /** Free-form comments (oldest first). */
  comments: Array<{ author: string; body: string; at: string }>;
}

// KV key shape — namespaced per appId so different projects don't
// step on each other's issue lists.
const issuesKey = (appId: string) => `issues:${appId}`;

function nextId(): string {
  // Cheap unique id — no uuid lib available server-side here. Random
  // suffix only needs to be stable for the lifetime of the issue (no
  // cross-tenant collision risk; keys are namespaced by appId).
  return `iss_${Math.random().toString(36).slice(2, 10)}`;
}

function seedIssues(appId: string): Issue[] {
  const t0 = new Date(Date.now() - 1000 * 60 * 60 * 24).toISOString();
  const t1 = new Date(Date.now() - 1000 * 60 * 60 * 2).toISOString();
  const t2 = new Date(Date.now() - 1000 * 60 * 30).toISOString();
  return [
    {
      id: nextId(),
      title: "Welcome to your project plan",
      description:
        "Issues land here from the PM agent, the SRE agent, the Critic, and you. " +
        "This one is a hello-world — close it whenever you like.",
      status: "open",
      source: "pm",
      assignee: "PM",
      created_at: t0,
      updated_at: t0,
      comments: [],
    },
    {
      id: nextId(),
      title: "Wire up password reset",
      description:
        "The forgot-password page exists, but the backing reset endpoint " +
        "is not wired up yet.",
      status: "in_progress",
      source: "builder",
      assignee: "Builder",
      created_at: t1,
      updated_at: t1,
      comments: [
        {
          author: "Builder",
          body: "Started. UI surface exists; backing endpoint still pending.",
          at: t1,
        },
      ],
    },
    {
      id: nextId(),
      title: "Initial deploy",
      description:
        "First successful build of the project sandbox — the deploy hash is " +
        "tracked on the app record.",
      status: "done",
      source: "you",
      assignee: null,
      created_at: t2,
      updated_at: t2,
      comments: [],
    },
  ];
  void appId;
}

/**
 * Read-or-seed: pull the per-appId issue list from KV; if missing,
 * synthesise the starter set, persist it, and return. Keeps issue
 * autocomplete from starting empty on a brand-new project before the
 * PM agent has filed anything.
 */
async function loadIssuesOrSeed(appId: string): Promise<Issue[]> {
  const existing = await persistGet<Issue[] | null>(issuesKey(appId), null);
  if (existing && Array.isArray(existing)) return existing;
  const seeded = seedIssues(appId);
  await persistSet(issuesKey(appId), seeded);
  return seeded;
}

export interface ListIssuesInput { appId: string }
export interface ListIssuesResult { issues: Issue[] }

export const listIssues = mutation(async (
  input: ListIssuesInput,
): Promise<ListIssuesResult> => {
  const issues = await loadIssuesOrSeed(input.appId);
  // Return a shallow copy so the caller can't mutate our store by
  // accident through a shared reference.
  return { issues: issues.map((i) => ({ ...i, comments: [...i.comments] })) };
}, { id: "agents.issues.list", input: appIdInputSchema, maxInputBytes: 4_096 });

// ─── quality scorecard ──────────────────────────────────────────
//
// Spec §11.1 names seven quality dimensions. KV-backed per appId via
// `internal/persist.ts`. The Critic loop now writes here on every round (see
// `internal/middleware.ts` data-critic-round handler — the middleware fires
// `setQualityScores` via `waitUntil()` after extracting the round
// payload). PM/SRE workers read via `getQualityScores`. Fresh apps
// without a Critic round yet fall back to the default snapshot so
// callers never see an empty scorecard.

export type QualityGrade =
  | "A+" | "A" | "A-"
  | "B+" | "B" | "B-"
  | "C+" | "C" | "C-"
  | "D" | "F";

export interface QualityDimension {
  key: string;
  label: string;
  grade: QualityGrade;
  rationale: string;
}

export interface QualityScores {
  overall: QualityGrade;
  dimensions: QualityDimension[];
  /** ISO-8601 of the last scorecard run. null = never scored. */
  last_run_at: string | null;
}

const qualityKey = (appId: string) => `quality:${appId}`;

function defaultScores(): QualityScores {
  return {
    overall: "B+",
    last_run_at: null,
    dimensions: [
      {
        key: "correctness",
        label: "Correctness",
        grade: "A",
        rationale: "Compiles cleanly. Smoke test passes.",
      },
      {
        key: "security",
        label: "Security",
        grade: "B+",
        rationale: "No secrets in source. CVE scan pending.",
      },
      {
        key: "performance",
        label: "Performance",
        grade: "B",
        rationale: "Bundle size unmeasured. LCP unmeasured.",
      },
      {
        key: "accessibility",
        label: "Accessibility",
        grade: "B-",
        rationale: "axe scan pending. No alt-text audit yet.",
      },
      {
        key: "ux_completeness",
        label: "UX completeness",
        grade: "B",
        rationale: "Forms render. Empty states partially covered.",
      },
      {
        key: "responsive",
        label: "Responsive",
        grade: "B+",
        rationale: "Layout holds at common breakpoints.",
      },
      {
        key: "code_health",
        label: "Code health",
        grade: "A-",
        rationale: "Lint clean. Type coverage on the high side.",
      },
    ],
  };
}

export interface GetQualityScoresInput { appId: string }

export const getQualityScores = query(async (
  input: GetQualityScoresInput,
): Promise<QualityScores> => {
  const scores = await persistGet<QualityScores | null>(
    qualityKey(input.appId),
    null,
  );
  const safe = scores ?? defaultScores();
  // Defensive copy — caller shouldn't mutate KV-cached values.
  return {
    overall: safe.overall,
    last_run_at: safe.last_run_at,
    dimensions: safe.dimensions.map((d) => ({ ...d })),
  };
}, { id: "agents.quality.get", input: appIdInputSchema, maxInputBytes: 4_096 });

// Note: the writer side of the quality scorecard (the function the
// chat middleware calls after every Critic round) lives in
// `internal/agent-writes.ts` so it stays out of the public RPC surface.
// Every export from this file becomes a network endpoint via `server.ts`'s
// `export *`; we want `setQualityFromCritic` to stay server-internal.
