"use server";
// Agents server functions — current stubs for the Plan / Health
// canvases.
//
// The PM agent owns Issues / Roadmap / Deployments (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.8) and the
// SRE agent owns Status / Quality / Incidents / Performance (§9.9).
// None of these have backing tables yet; this file exposes a single-
// input RPC surface (`{appId, …}`) so the canvas shells render real-
// looking data while the real backing tables are still being built.
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
// `vite-plugin` `args[0]`-only forwarding honest even when we add
// `addIssue({appId, title, description})`.

import { query, mutation } from "@zeroship/rpc/server";
import { persistGet, persistSet } from "./internal/persist.js";
import {
  CRITIC_DIMENSION_LABELS,
  CRITIC_DIMENSIONS,
} from "../shared/review-contract.js";

// ─── issue store ────────────────────────────────────────────────

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

// KV key shape — namespaced per appId so different projects don't
// step on each other's issue lists.
const issuesKey = (appId: string) => `issues:${appId}`;

function nowIso(): string {
  return new Date().toISOString();
}

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

/**
 * Read-or-seed: pull the per-appId issue list from KV; if missing,
 * synthesise the starter set, persist it, and return. Keeps the canvas
 * from rendering an empty list on a brand-new project before the PM
 * agent has filed anything.
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
}, { id: "agents.listIssues" });

export interface AddIssueInput {
  appId: string;
  title: string;
  description: string;
  source?: IssueSource;
  assignee?: string | null;
}

export const addIssue = mutation(async (
  input: AddIssueInput,
): Promise<{ issue: Issue }> => {
  const list = await loadIssuesOrSeed(input.appId);
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
  // Newest first — matches the `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.8 mock where the most recent
  // issue sits at the top of the list.
  const next = [issue, ...list];
  await persistSet(issuesKey(input.appId), next);
  return { issue };
}, { id: "agents.addIssue" });

// ─── quality scorecard ──────────────────────────────────────────
//
// Spec §11.1 plus the UI design-flow gates define the quality dimensions.
// KV-backed per appId via
// `internal/persist.ts`. The Critic loop now writes here on every round (see
// `internal/middleware.ts` data-critic-round handler — the middleware fires
// `setQualityScores` via `waitUntil()` after extracting the round
// payload). HealthCanvas reads via `getQualityScores`. Fresh apps
// without a Critic round yet fall back to the default snapshot so
// the grid never renders blank.

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
    dimensions: CRITIC_DIMENSIONS.map((key) => ({
      key,
      label: CRITIC_DIMENSION_LABELS[key],
      grade: defaultGradeForDimension(key),
      rationale: defaultRationaleForDimension(key),
    })),
  };
}

function defaultGradeForDimension(key: string): QualityGrade {
  switch (key) {
    case "accessibility":
    case "states":
    case "content":
      return "B-";
    case "performance":
      return "B";
    case "security":
    case "responsive":
    case "composed-from-system":
      return "B+";
    case "code_health":
      return "A-";
    default:
      return "A";
  }
}

function defaultRationaleForDimension(key: string): string {
  switch (key) {
    case "composed-from-system":
      return "@zeroship/ui composition audit pending.";
    case "states":
      return "Empty/loading/error/success/partial state audit pending.";
    case "responsive":
      return "Layout holds at common breakpoints.";
    case "accessibility":
      return "WCAG AA audit pending.";
    case "content":
      return "Copy, validation, and empty-state guidance audit pending.";
    case "security":
      return "No secrets in source. CVE scan pending.";
    case "performance":
      return "Bundle size unmeasured. LCP unmeasured.";
    case "code_health":
      return "Lint clean. Type coverage on the high side.";
    default:
      return "Compiles cleanly. Smoke test passes.";
  }
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
}, { id: "agents.getQualityScores" });

// Note: the writer side of the quality scorecard (the function the
// chat middleware calls after every Critic round) lives in
// `internal/agent-writes.ts` so it stays out of the public RPC surface. Every
// export from this file becomes a network endpoint via `server.ts`'s
// `export *`; we want `setQualityFromCritic` to stay server-internal.

// ─── data canvas stubs ───────────────────────────────────────────
//
// V1 surface for the Data canvas (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.3). The control plane
// has no per-app introspection RPC yet — pg_catalog reads, table row
// pagination, index/migration/backup tracking all need backing schemas
// that do not exist yet.
//
// Same wire convention as the rest of this file: every export
// takes ONE object input. Returns hardcoded sample shapes shared
// across all appIds — that's enough for the canvas shells to
// render real-looking content while we wait for the real
// endpoints. Per-appId variation isn't useful here because nothing
// the user does in V1 actually mutates this stub state.

export interface TableSummary {
  /** Schema-qualified or bare table name (matches pg_catalog). */
  name: string;
  /** Approximate row count (would come from pg_class.reltuples). */
  row_count: number;
  /** Rough on-disk size in bytes (would come from pg_total_relation_size). */
  size_bytes: number;
  /** ISO-8601 of the last write (would come from per-table pg_stat). */
  updated_at: string;
}

export interface ListTablesInput { appId: string }
export interface ListTablesResult { tables: TableSummary[] }

const SAMPLE_TABLES: ReadonlyArray<TableSummary> = [
  {
    name: "users",
    row_count: 142,
    size_bytes: 32_768,
    updated_at: new Date(Date.now() - 1000 * 60 * 12).toISOString(),
  },
  {
    name: "posts",
    row_count: 487,
    size_bytes: 196_608,
    updated_at: new Date(Date.now() - 1000 * 60 * 2).toISOString(),
  },
  {
    name: "comments",
    row_count: 1_204,
    size_bytes: 524_288,
    updated_at: new Date(Date.now() - 1000 * 30).toISOString(),
  },
];

export const listTables = query(async (
  input: ListTablesInput,
): Promise<ListTablesResult> => {
  void input.appId;
  // Defensive copy so callers can't mutate the constant.
  return { tables: SAMPLE_TABLES.map((t) => ({ ...t })) };
}, { id: "agents.listTables" });

export interface TableRow {
  /** Stable row id used as a React key. */
  id: string;
  /** Cell values keyed by column name. Strings only in V1. */
  cells: Record<string, string>;
}

export interface GetTableRowsInput {
  appId: string;
  tableName: string;
  limit?: number;
  offset?: number;
}
export interface GetTableRowsResult {
  /** Column names in display order. */
  columns: string[];
  rows: TableRow[];
  /** Total rows in the table (for pagination footer). */
  total: number;
  /** Echoed back so the client can sanity-check pagination state. */
  limit: number;
  offset: number;
}

interface SampleColumns {
  columns: string[];
  rows: TableRow[];
}

const SAMPLE_ROWS: Record<string, SampleColumns> = {
  users: {
    columns: ["id", "email", "name", "created_at"],
    rows: Array.from({ length: 27 }, (_, i) => ({
      id: `usr_${(i + 1).toString().padStart(4, "0")}`,
      cells: {
        id: `usr_${(i + 1).toString().padStart(4, "0")}`,
        email: `user${i + 1}@example.com`,
        name: `User ${i + 1}`,
        created_at: new Date(Date.now() - 1000 * 60 * 60 * (i + 1)).toISOString(),
      },
    })),
  },
  posts: {
    columns: ["id", "title", "author_id", "published_at"],
    rows: Array.from({ length: 18 }, (_, i) => ({
      id: `post_${(i + 1).toString().padStart(4, "0")}`,
      cells: {
        id: `post_${(i + 1).toString().padStart(4, "0")}`,
        title: `Sample post ${i + 1}`,
        author_id: `usr_${((i % 5) + 1).toString().padStart(4, "0")}`,
        published_at: new Date(Date.now() - 1000 * 60 * 30 * (i + 1)).toISOString(),
      },
    })),
  },
  comments: {
    columns: ["id", "post_id", "author_id", "body"],
    rows: Array.from({ length: 33 }, (_, i) => ({
      id: `cmt_${(i + 1).toString().padStart(4, "0")}`,
      cells: {
        id: `cmt_${(i + 1).toString().padStart(4, "0")}`,
        post_id: `post_${((i % 8) + 1).toString().padStart(4, "0")}`,
        author_id: `usr_${((i % 5) + 1).toString().padStart(4, "0")}`,
        body: `Sample comment body ${i + 1} — pretend this is meaningful.`,
      },
    })),
  },
};

export const getTableRows = query(async (
  input: GetTableRowsInput,
): Promise<GetTableRowsResult> => {
  void input.appId;
  const limit = Math.max(1, Math.min(200, input.limit ?? 25));
  const offset = Math.max(0, input.offset ?? 0);
  const sample = SAMPLE_ROWS[input.tableName];
  if (!sample) {
    return { columns: [], rows: [], total: 0, limit, offset };
  }
  const slice = sample.rows
    .slice(offset, offset + limit)
    .map((r) => ({ id: r.id, cells: { ...r.cells } }));
  return {
    columns: [...sample.columns],
    rows: slice,
    total: sample.rows.length,
    limit,
    offset,
  };
}, { id: "agents.getTableRows" });

export interface IndexInfo {
  table: string;
  name: string;
  type: "btree" | "hash" | "gin" | "gist" | "unique";
  columns: string[];
  /** Approximate on-disk size in bytes. */
  size_bytes: number;
  /** ISO-8601 of last reported use, null if never. */
  last_used: string | null;
}

export interface ListIndexesInput { appId: string }
export interface ListIndexesResult { indexes: IndexInfo[] }

const SAMPLE_INDEXES: ReadonlyArray<IndexInfo> = [
  {
    table: "users",
    name: "users_pkey",
    type: "btree",
    columns: ["id"],
    size_bytes: 16_384,
    last_used: new Date(Date.now() - 1000 * 60 * 2).toISOString(),
  },
  {
    table: "users",
    name: "users_email_uk",
    type: "unique",
    columns: ["email"],
    size_bytes: 24_576,
    last_used: new Date(Date.now() - 1000 * 60 * 5).toISOString(),
  },
  {
    table: "posts",
    name: "posts_pkey",
    type: "btree",
    columns: ["id"],
    size_bytes: 32_768,
    last_used: new Date(Date.now() - 1000 * 60 * 3).toISOString(),
  },
  {
    table: "posts",
    name: "posts_author_idx",
    type: "btree",
    columns: ["author_id"],
    size_bytes: 28_672,
    last_used: new Date(Date.now() - 1000 * 60 * 60 * 2).toISOString(),
  },
  {
    table: "comments",
    name: "comments_post_idx",
    type: "btree",
    columns: ["post_id"],
    size_bytes: 49_152,
    last_used: new Date(Date.now() - 1000 * 60 * 1).toISOString(),
  },
];

export const listIndexes = query(async (
  input: ListIndexesInput,
): Promise<ListIndexesResult> => {
  void input.appId;
  return { indexes: SAMPLE_INDEXES.map((i) => ({ ...i, columns: [...i.columns] })) };
}, { id: "agents.listIndexes" });

export type MigrationStatus = "applied" | "pending" | "failed";

export interface MigrationEntry {
  id: string;
  name: string;
  status: MigrationStatus;
  /** Display name of who/what filed the migration. */
  author: string;
  /** ISO-8601 of when it was applied (or attempted). */
  at: string;
}

export interface ListMigrationsInput { appId: string }
export interface ListMigrationsResult { migrations: MigrationEntry[] }

const SAMPLE_MIGRATIONS: ReadonlyArray<MigrationEntry> = [
  {
    id: "20260430.142512",
    name: "add password reset",
    status: "applied",
    author: "Builder",
    at: new Date(Date.now() - 1000 * 60 * 4).toISOString(),
  },
  {
    id: "20260430.140003",
    name: "add stage_id index",
    status: "applied",
    author: "Builder",
    at: new Date(Date.now() - 1000 * 60 * 8).toISOString(),
  },
  {
    id: "20260429.180000",
    name: "drop legacy_field",
    status: "pending",
    author: "Builder",
    at: new Date(Date.now() - 1000 * 60 * 60 * 26).toISOString(),
  },
  {
    id: "20260428.090000",
    name: "initial schema",
    status: "applied",
    author: "Builder",
    at: new Date(Date.now() - 1000 * 60 * 60 * 24 * 2).toISOString(),
  },
];

export const listMigrations = query(async (
  input: ListMigrationsInput,
): Promise<ListMigrationsResult> => {
  void input.appId;
  return { migrations: SAMPLE_MIGRATIONS.map((m) => ({ ...m })) };
}, { id: "agents.listMigrations" });

export type BackupKind = "auto" | "manual";

export interface BackupEntry {
  id: string;
  /** Human-readable label — "Daily auto" or a manual snapshot name. */
  label: string;
  kind: BackupKind;
  /** Approximate size in bytes. */
  size_bytes: number;
  /** ISO-8601 of when the snapshot was taken. */
  at: string;
}

const backupsKey = (appId: string) => `backups:${appId}`;

function seedBackups(): BackupEntry[] {
  return [
    {
      id: `bk_${Math.random().toString(36).slice(2, 10)}`,
      label: "Daily auto",
      kind: "auto",
      size_bytes: 4_509_715_660,
      at: new Date(Date.now() - 1000 * 60 * 60 * 17).toISOString(),
    },
    {
      id: `bk_${Math.random().toString(36).slice(2, 10)}`,
      label: "Daily auto",
      kind: "auto",
      size_bytes: 4_402_341_990,
      at: new Date(Date.now() - 1000 * 60 * 60 * 24).toISOString(),
    },
    {
      id: `bk_${Math.random().toString(36).slice(2, 10)}`,
      label: "before-payment-mig",
      kind: "manual",
      size_bytes: 4_402_341_990,
      at: new Date(Date.now() - 1000 * 60 * 60 * 24 * 2).toISOString(),
    },
  ];
}

async function loadBackupsOrSeed(appId: string): Promise<BackupEntry[]> {
  const existing = await persistGet<BackupEntry[] | null>(
    backupsKey(appId),
    null,
  );
  if (existing && Array.isArray(existing)) return existing;
  const seeded = seedBackups();
  await persistSet(backupsKey(appId), seeded);
  return seeded;
}

export interface ListBackupsInput { appId: string }
export interface ListBackupsResult { backups: BackupEntry[] }

export const listBackups = mutation(async (
  input: ListBackupsInput,
): Promise<ListBackupsResult> => {
  const list = await loadBackupsOrSeed(input.appId);
  return { backups: list.map((b) => ({ ...b })) };
}, { id: "agents.listBackups" });

export interface TriggerBackupInput { appId: string }
export interface TriggerBackupResult { backup: BackupEntry }

export const triggerBackup = mutation(async (
  input: TriggerBackupInput,
): Promise<TriggerBackupResult> => {
  const list = await loadBackupsOrSeed(input.appId);
  const entry: BackupEntry = {
    id: `bk_${Math.random().toString(36).slice(2, 10)}`,
    label: "Manual snapshot",
    kind: "manual",
    // Cheap pseudo-random size in the same order of magnitude as the
    // seed entries so the UI table doesn't look out of place.
    size_bytes: 4_400_000_000 + Math.floor(Math.random() * 200_000_000),
    at: nowIso(),
  };
  // Newest first matches the `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.3.5 mock.
  const next = [entry, ...list];
  await persistSet(backupsKey(input.appId), next);
  return { backup: { ...entry } };
}, { id: "agents.triggerBackup" });

// ─── media canvas stubs ──────────────────────────────────────────
//
// V1 surface for the Media canvas (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.4). Object Storage exists
// in the platform (`zeroship.storage.*` + `@zeroship/storage`) but
// there's no per-app upload RPC exposed to the dashboard yet — we'd
// need a control-plane endpoint that scopes uploads to the caller's
// app and returns a public URL. Until that lands, this Map keeps
// uploaded files in-process; they survive the dev session but vanish
// on isolate eviction.

export type MediaContentClass = "image" | "video" | "audio" | "other";

export interface MediaEntry {
  /** Storage key — opaque, used for delete. */
  key: string;
  /** Original filename for display. */
  name: string;
  /** Size in bytes. */
  size: number;
  /** MIME type as reported by the uploader. */
  contentType: string;
  /** URL the dashboard can render. data: URL for in-memory uploads. */
  url: string;
  /** ISO-8601 upload time. */
  uploaded_at: string;
}

const mediaKey = (appId: string) => `media:${appId}`;

// Cap inline base64 uploads at 1 MB so a stray "drag-everything-onto-
// the-canvas" doesn't blow up the KV store. A real storage backend
// should live behind a control-plane endpoint and accept arbitrary
// sizes via streaming multipart.
const MAX_UPLOAD_BYTES = 1_048_576;

function seedMedia(): MediaEntry[] {
  // Three editorial sample tiles so the canvas isn't an empty rectangle
  // on first paint. Two images (placeholder URLs from picsum.photos
  // resolve in dev) + one non-image so the file-icon path is exercised.
  const t = Date.now();
  return [
    {
      key: "seed_logo",
      name: "logo.png",
      size: 24_576,
      contentType: "image/png",
      url: "https://picsum.photos/seed/logo/200/200",
      uploaded_at: new Date(t - 1000 * 60 * 60 * 24).toISOString(),
    },
    {
      key: "seed_hero",
      name: "hero.jpg",
      size: 198_432,
      contentType: "image/jpeg",
      url: "https://picsum.photos/seed/hero/200/200",
      uploaded_at: new Date(t - 1000 * 60 * 60 * 6).toISOString(),
    },
    {
      key: "seed_brief",
      name: "brand-brief.pdf",
      size: 412_672,
      contentType: "application/pdf",
      url: "data:application/pdf;base64,",
      uploaded_at: new Date(t - 1000 * 60 * 30).toISOString(),
    },
  ];
}

async function loadMediaOrSeed(appId: string): Promise<MediaEntry[]> {
  const existing = await persistGet<MediaEntry[] | null>(mediaKey(appId), null);
  if (existing && Array.isArray(existing)) return existing;
  const seeded = seedMedia();
  await persistSet(mediaKey(appId), seeded);
  return seeded;
}

export interface ListMediaInput { appId: string }
export interface ListMediaResult { items: MediaEntry[] }

export const listMedia = mutation(async (
  input: ListMediaInput,
): Promise<ListMediaResult> => {
  const list = await loadMediaOrSeed(input.appId);
  return { items: list.map((m) => ({ ...m })) };
}, { id: "agents.listMedia" });

export interface UploadMediaInput {
  appId: string;
  name: string;
  contentType: string;
  /** Base64-encoded file bytes (no `data:` prefix). */
  base64: string;
}
export interface UploadMediaResult { item: MediaEntry }

export const uploadMedia = mutation(async (
  input: UploadMediaInput,
): Promise<UploadMediaResult> => {
  const list = await loadMediaOrSeed(input.appId);
  // Best-effort byte length — `Buffer` is available in the V8 runtime
  // via the node-compat shim, but we fall back to the base64 string
  // length × 3/4 if not. Either way the size is approximate.
  let size: number;
  try {
    size = Buffer.from(input.base64, "base64").byteLength;
  } catch {
    size = Math.floor((input.base64.length * 3) / 4);
  }
  if (size > MAX_UPLOAD_BYTES) {
    throw new Error(
      `File too large for the V1 KV-backed media store (${size} bytes; cap is ${MAX_UPLOAD_BYTES}). ` +
        `A persistent storage backend with streaming uploads is not wired yet.`,
    );
  }
  const key = `m_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
  const entry: MediaEntry = {
    key,
    name: input.name || "untitled",
    size,
    contentType: input.contentType || "application/octet-stream",
    // For now we serve uploads back as data URLs so the preview thumb
    // works without a backing object store. A real storage backend can
    // replace this later.
    url: `data:${input.contentType || "application/octet-stream"};base64,${input.base64}`,
    uploaded_at: nowIso(),
  };
  const next = [entry, ...list];
  await persistSet(mediaKey(input.appId), next);
  return { item: { ...entry } };
}, { id: "agents.uploadMedia" });

export interface DeleteMediaInput {
  appId: string;
  key: string;
}
export interface DeleteMediaResult { ok: true }

export const deleteMedia = mutation(async (
  input: DeleteMediaInput,
): Promise<DeleteMediaResult> => {
  const list = await loadMediaOrSeed(input.appId);
  const next = list.filter((m) => m.key !== input.key);
  await persistSet(mediaKey(input.appId), next);
  return { ok: true };
}, { id: "agents.deleteMedia" });
