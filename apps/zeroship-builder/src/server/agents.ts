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

// ─── data canvas stubs (ISS-20 → ISS-25) ────────────────────────
//
// V1 surface for the Data canvas (spec §9.3). The control plane
// has no per-app introspection RPC yet — pg_catalog reads, table
// row pagination, index/migration/backup tracking all need backing
// schemas that don't exist. Each ISSUES.md entry below names the
// missing piece.
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

export async function listTables(
  input: ListTablesInput,
): Promise<ListTablesResult> {
  void input.appId;
  // Defensive copy so callers can't mutate the constant.
  return { tables: SAMPLE_TABLES.map((t) => ({ ...t })) };
}
listTables.config = { id: "agents.listTables" };

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

export async function getTableRows(
  input: GetTableRowsInput,
): Promise<GetTableRowsResult> {
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
}
getTableRows.config = { id: "agents.getTableRows" };

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

export async function listIndexes(
  input: ListIndexesInput,
): Promise<ListIndexesResult> {
  void input.appId;
  return { indexes: SAMPLE_INDEXES.map((i) => ({ ...i, columns: [...i.columns] })) };
}
listIndexes.config = { id: "agents.listIndexes" };

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

export async function listMigrations(
  input: ListMigrationsInput,
): Promise<ListMigrationsResult> {
  void input.appId;
  return { migrations: SAMPLE_MIGRATIONS.map((m) => ({ ...m })) };
}
listMigrations.config = { id: "agents.listMigrations" };

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

const BACKUPS = new Map<string, BackupEntry[]>();

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

function ensureBackups(appId: string): BackupEntry[] {
  let list = BACKUPS.get(appId);
  if (!list) {
    list = seedBackups();
    BACKUPS.set(appId, list);
  }
  return list;
}

export interface ListBackupsInput { appId: string }
export interface ListBackupsResult { backups: BackupEntry[] }

export async function listBackups(
  input: ListBackupsInput,
): Promise<ListBackupsResult> {
  const list = ensureBackups(input.appId);
  return { backups: list.map((b) => ({ ...b })) };
}
listBackups.config = { id: "agents.listBackups" };

export interface TriggerBackupInput { appId: string }
export interface TriggerBackupResult { backup: BackupEntry }

export async function triggerBackup(
  input: TriggerBackupInput,
): Promise<TriggerBackupResult> {
  const list = ensureBackups(input.appId);
  const entry: BackupEntry = {
    id: `bk_${Math.random().toString(36).slice(2, 10)}`,
    label: "Manual snapshot",
    kind: "manual",
    // Cheap pseudo-random size in the same order of magnitude as the
    // seed entries so the UI table doesn't look out of place.
    size_bytes: 4_400_000_000 + Math.floor(Math.random() * 200_000_000),
    at: nowIso(),
  };
  // Newest first matches the spec §9.3.5 mock.
  list.unshift(entry);
  return { backup: { ...entry } };
}
triggerBackup.config = { id: "agents.triggerBackup" };

// ─── media canvas stubs (ISS-26) ────────────────────────────────
//
// V1 surface for the Media canvas (spec §9.4). Object Storage exists
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

const MEDIA = new Map<string, MediaEntry[]>();

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

function ensureMedia(appId: string): MediaEntry[] {
  let list = MEDIA.get(appId);
  if (!list) {
    list = seedMedia();
    MEDIA.set(appId, list);
  }
  return list;
}

export interface ListMediaInput { appId: string }
export interface ListMediaResult { items: MediaEntry[] }

export async function listMedia(
  input: ListMediaInput,
): Promise<ListMediaResult> {
  const list = ensureMedia(input.appId);
  return { items: list.map((m) => ({ ...m })) };
}
listMedia.config = { id: "agents.listMedia" };

export interface UploadMediaInput {
  appId: string;
  name: string;
  contentType: string;
  /** Base64-encoded file bytes (no `data:` prefix). */
  base64: string;
}
export interface UploadMediaResult { item: MediaEntry }

export async function uploadMedia(
  input: UploadMediaInput,
): Promise<UploadMediaResult> {
  const list = ensureMedia(input.appId);
  // Best-effort byte length — `Buffer` is available in the V8 runtime
  // via the node-compat shim, but we fall back to the base64 string
  // length × 3/4 if not. Either way the size is approximate.
  let size: number;
  try {
    size = Buffer.from(input.base64, "base64").byteLength;
  } catch {
    size = Math.floor((input.base64.length * 3) / 4);
  }
  const key = `m_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
  const entry: MediaEntry = {
    key,
    name: input.name || "untitled",
    size,
    contentType: input.contentType || "application/octet-stream",
    // For now we serve uploads back as data URLs so the preview thumb
    // works without a backing object store. Real Storage backend lives
    // behind ISS-26.
    url: `data:${input.contentType || "application/octet-stream"};base64,${input.base64}`,
    uploaded_at: nowIso(),
  };
  list.unshift(entry);
  return { item: { ...entry } };
}
uploadMedia.config = { id: "agents.uploadMedia" };

export interface DeleteMediaInput {
  appId: string;
  key: string;
}
export interface DeleteMediaResult { ok: true }

export async function deleteMedia(
  input: DeleteMediaInput,
): Promise<DeleteMediaResult> {
  const list = ensureMedia(input.appId);
  const idx = list.findIndex((m) => m.key === input.key);
  if (idx >= 0) list.splice(idx, 1);
  return { ok: true };
}
deleteMedia.config = { id: "agents.deleteMedia" };
