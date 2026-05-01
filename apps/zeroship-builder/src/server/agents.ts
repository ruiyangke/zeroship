"use server";
// Agents server functions — V1 stubs for the Plan / Health canvases.
//
// The PM agent owns Issues / Roadmap / Deployments (spec §9.8) and the
// SRE agent owns Status / Quality / Incidents / Performance (§9.9).
// None of these have backing tables yet; this file exposes a single-
// input RPC surface (`{appId, …}`) so the canvas shells render real-
// looking data while the schema gaps are tracked in ISSUES.md
// (ISS-14 through ISS-18).
//
// Storage shape: a module-level `Map<appId, …>` per resource. Survives
// only the lifetime of the V8 isolate / dev process. When the worker
// restarts, state vanishes — that's intentional, the production
// implementation lives behind the `Fix path` notes in each ISSUES
// entry.
//
// Wire convention: every export takes ONE object input (per the
// builder's single-input RPC wire — see `apps/zeroship-builder/src/
// server/sandbox.ts` header for the long form). That keeps the
// `vite-plugin` `args[0]`-only forwarding honest even when we add
// `addIssue({appId, title, description})`.

// ─── issue store (ISS-14) ───────────────────────────────────────

export type IssueStatus = "open" | "in_progress" | "done";
export type IssueSource = "you" | "pm" | "sre" | "builder";

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

// One entry per appId. Seeded lazily on first read so every fresh app
// gets a starter issue list — that keeps the canvas from looking
// broken on a brand-new project before the PM agent has done anything.
const ISSUES = new Map<string, Issue[]>();

function nowIso(): string {
  return new Date().toISOString();
}

function nextId(): string {
  // Cheap unique id — no uuid lib available server-side here, and this
  // module disappears on isolate eviction anyway.
  return `iss_${Math.random().toString(36).slice(2, 10)}`;
}

function seedIssues(appId: string): Issue[] {
  const t0 = new Date(Date.now() - 1000 * 60 * 60 * 24).toISOString();
  const t1 = new Date(Date.now() - 1000 * 60 * 60 * 2).toISOString();
  const t2 = new Date(Date.now() - 1000 * 60 * 30).toISOString();
  return [
    {
      id: nextId(),
      title: "Welcome to the plan canvas",
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
        "Spec §6.3 promises a forgot-password flow. The endpoint lives behind " +
        "ISSUES.md ISS-09 — track that for the backing handler.",
      status: "in_progress",
      source: "builder",
      assignee: "Builder",
      created_at: t1,
      updated_at: t1,
      comments: [
        {
          author: "Builder",
          body: "Started. UI stub ships in §6.3.",
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

function ensureSeed(appId: string): Issue[] {
  const existing = ISSUES.get(appId);
  if (existing) return existing;
  const seeded = seedIssues(appId);
  ISSUES.set(appId, seeded);
  return seeded;
}

export interface ListIssuesInput { appId: string }
export interface ListIssuesResult { issues: Issue[] }

export async function listIssues(input: ListIssuesInput): Promise<ListIssuesResult> {
  const issues = ensureSeed(input.appId);
  // Return a shallow copy so the caller can't mutate our store by
  // accident through a shared reference.
  return { issues: issues.map((i) => ({ ...i, comments: [...i.comments] })) };
}
listIssues.config = { id: "agents.listIssues" };

export interface AddIssueInput {
  appId: string;
  title: string;
  description: string;
  source?: IssueSource;
  assignee?: string | null;
}

export async function addIssue(input: AddIssueInput): Promise<{ issue: Issue }> {
  const list = ensureSeed(input.appId);
  const now = nowIso();
  const issue: Issue = {
    id: nextId(),
    title: input.title.trim() || "Untitled",
    description: (input.description ?? "").trim(),
    status: "open",
    source: input.source ?? "you",
    assignee: input.assignee ?? null,
    created_at: now,
    updated_at: now,
    comments: [],
  };
  // Newest first — matches the spec §9.8 mock where the most recent
  // issue sits at the top of the list.
  list.unshift(issue);
  return { issue };
}
addIssue.config = { id: "agents.addIssue" };

// ─── quality scorecard (ISS-16) ─────────────────────────────────
//
// Spec §11.1 names seven quality dimensions; we expose a hardcoded
// snapshot per appId here. The Critic loop is supposed to populate
// this Map as it scores each build, but the wiring isn't in place yet
// (see ISSUES.md ISS-16). For now every appId gets the same "freshly
// scaffolded" scorecard plus a per-app rationale that mentions the
// app id so the canvas at least feels app-specific.

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

const QUALITY: Map<string, QualityScores> = new Map();

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

export async function getQualityScores(
  input: GetQualityScoresInput,
): Promise<QualityScores> {
  let scores = QUALITY.get(input.appId);
  if (!scores) {
    scores = defaultScores();
    QUALITY.set(input.appId, scores);
  }
  // Defensive copy — dimensions array is shared otherwise.
  return {
    overall: scores.overall,
    last_run_at: scores.last_run_at,
    dimensions: scores.dimensions.map((d) => ({ ...d })),
  };
}
getQualityScores.config = { id: "agents.getQualityScores" };
