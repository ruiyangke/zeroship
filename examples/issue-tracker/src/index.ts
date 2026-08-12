"use server";

// Bugzilla-faithful issue tracker RPC layer.
//
// The schema is deliberately NOT declared here. The committed migrations are
// folded by @zeroship/vite-plugin into generated/zeroship/schema.runtime.json,
// and the runtime installs the resulting collections on env.db before this
// module is evaluated.

import { env } from "zeroship";
import { mutation, query } from "@zeroship/rpc/server";
import { diffTrackedFields, type TrackedFieldChange } from "./lib/changes";
import { assertFlagTarget } from "./lib/flags";
import {
  weaklyConnectedComponent,
  wouldCreateDirectedCycle,
  type DirectedEdge,
} from "./lib/graph";
import {
  BUG_PRIORITIES,
  BUG_SEVERITIES,
  parseQuickSearch,
  type BugPriority,
  type BugSeverity,
  type QuickSearchClause,
} from "./lib/quicksearch";
import {
  BUG_RESOLUTIONS,
  BUG_STATUSES,
  isBugResolution,
  isBugStatus,
  markDuplicateBugState,
  reopenBugState,
  resolveBugState,
  transitionBugState,
  type BugResolution,
  type BugState,
  type BugStatus,
} from "./lib/workflow";

type EmptyInput = Record<string, never>;
type DbFilter = Record<string, unknown>;
type DbPatch = Record<string, unknown>;
type DbResult<T> =
  | { data: T; error: null }
  | { data: null; error: Error };

type SystemRow = {
  id: string;
  created_at: number;
  updated_at: number;
  created_by: string | null;
  updated_by: string | null;
  version: number;
  deleted_at: number | null;
};

type UserRow = SystemRow & {
  email: string;
  handle: string;
  name: string;
  timezone: string;
  isAdmin: boolean;
  isDisabled: boolean;
  // Free-form user preferences. Bugzilla keeps login + realname and nothing
  // else on the account row; everything else it calls a "preference" lives in
  // a bag, which is what this is. `users.updatePrefs` had no storage at all
  // before this column existed.
  prefs?: Record<string, unknown> | null;
};

type ProductRow = SystemRow & {
  name: string;
  description?: string | null;
  classification: string;
  defaultMilestone?: string | null;
  allowsUnconfirmed: boolean;
  isActive: boolean;
  // Bugzilla voting limits. Present in the schema since the rework and
  // absent from this view, which is why nothing could read them.
  votesPerUser: number;
  maxVotesPerBug: number;
  votesToConfirm: number;
};

type ComponentRow = SystemRow & {
  productId: string;
  name: string;
  description?: string | null;
  defaultAssigneeId?: string | null;
  defaultQaContactId?: string | null;
  isActive: boolean;
};

type VersionRow = SystemRow & {
  productId: string;
  name: string;
  sortKey: number;
  isActive: boolean;
};

type MilestoneRow = VersionRow;

type BugRow = SystemRow & {
  productId: string;
  componentId: string;
  versionId?: string | null;
  milestoneId?: string | null;
  summary: string;
  status: string;
  resolution?: string | null;
  severity: string;
  priority: string;
  reporterId: string;
  assigneeId?: string | null;
  qaContactId?: string | null;
  duplicateOfId?: string | null;
  whiteboard?: string | null;
  opSys: string;
  platform: string;
  url?: string | null;
  isConfirmed: boolean;
  voteCount: number;
  commentCount: number;
  estimatedTimeMinutes?: number;
  remainingTimeMinutes?: number;
  deadline?: number | null;
  // Stamped by bugs.resolve / bugs.markDuplicate and cleared by bugs.reopen.
  // reports.timeToResolve reads this instead of mining the activities table
  // for status->RESOLVED transitions, which was a scan plus string matching.
  resolvedAt?: number | null;
};

type CommentRow = SystemRow & {
  bugId: string;
  authorId: string;
  body: string;
  commentNumber: number;
  isPrivate: boolean;
  workTimeMinutes: number;
};

type AttachmentRow = SystemRow & {
  bugId: string;
  uploaderId: string;
  filename: string;
  contentType: string;
  sizeBytes: number;
  storageKey: string;
  description?: string | null;
  isPatch: boolean;
  isObsolete: boolean;
};

type KeywordRow = SystemRow & {
  name: string;
  description?: string | null;
};

type BugKeywordRow = SystemRow & { bugId: string; keywordId: string };
type DependencyRow = SystemRow & { bugId: string; dependsOnId: string };
type CcRow = SystemRow & { bugId: string; userId: string };
type SeeAlsoRow = SystemRow & { bugId: string; url: string };
type WatcherRow = SystemRow & { watcherId: string; watchedId: string };
type VoteRow = SystemRow & { bugId: string; userId: string; count: number };
type ProductGroupRow = SystemRow & { productId: string; groupId: string };
type GroupMemberRow = SystemRow & { groupId: string; userId: string };
type GroupRow = SystemRow & {
  name: string;
  description?: string | null;
  isBugGroup: boolean;
};
type BugGroupRow = SystemRow & { bugId: string; groupId: string };

type FlagTypeRow = SystemRow & {
  name: string;
  description?: string | null;
  targetType: string;
  isRequestable: boolean;
  isMultiplicable: boolean;
  productId?: string | null;
};

type FlagRow = SystemRow & {
  flagTypeId: string;
  bugId?: string | null;
  attachmentId?: string | null;
  setterId: string;
  requesteeId?: string | null;
  status: string;
};

type ActivityRow = SystemRow & {
  bugId: string;
  actorId: string;
  fieldName: string;
  oldValue?: string | null;
  newValue?: string | null;
};

type SavedSearchRow = SystemRow & {
  ownerId: string;
  name: string;
  queryJson: unknown;
  isShared: boolean;
};

type NotificationRow = SystemRow & {
  userId: string;
  bugId?: string | null;
  kind: string;
  title: string;
  body?: string | null;
  isRead: boolean;
};

interface ReadQuery<Row> extends PromiseLike<DbResult<Row[]>> {
  sort(order: Record<string, 1 | -1>): ReadQuery<Row>;
  limit(count: number): ReadQuery<Row>;
  skip(count: number): ReadQuery<Row>;
  after(id: string): ReadQuery<Row>;
  first(): Promise<DbResult<Row | null>>;
}

interface Collection<Row> {
  get(idOrFilter: string | DbFilter): Promise<DbResult<Row | null>>;
  find(filter?: DbFilter): ReadQuery<Row>;
  insert(row: Record<string, unknown>): Promise<DbResult<Row>>;
  insertMany(rows: Record<string, unknown>[]): Promise<DbResult<Row[]>>;
  update(idOrFilter: string | DbFilter, patch: DbPatch): Promise<DbResult<Row | null>>;
  delete(idOrFilter: string | DbFilter): Promise<DbResult<Row | null>>;
  deleteMany(filter: DbFilter): Promise<DbResult<{ deletedCount: number }>>;
  count(filter?: DbFilter): Promise<DbResult<number>>;
}

interface TxQuery<Row> extends PromiseLike<Row[]> {
  sort(order: Record<string, 1 | -1>): TxQuery<Row>;
  limit(count: number): TxQuery<Row>;
  skip(count: number): TxQuery<Row>;
  after(id: string): TxQuery<Row>;
  first(): Promise<Row | null>;
}

interface TxCollection<Row> {
  get(idOrFilter: string | DbFilter): Promise<Row | null>;
  find(filter?: DbFilter): TxQuery<Row>;
  insert(row: Record<string, unknown>): Promise<Row>;
  insertMany(rows: Record<string, unknown>[]): Promise<Row[]>;
  update(idOrFilter: string | DbFilter, patch: DbPatch): Promise<Row | null>;
  delete(idOrFilter: string | DbFilter): Promise<Row | null>;
  deleteMany(filter: DbFilter): Promise<{ deletedCount: number }>;
  count(filter?: DbFilter): Promise<number>;
}

type TxDb = {
  users: TxCollection<UserRow>;
  products: TxCollection<ProductRow>;
  components: TxCollection<ComponentRow>;
  versions: TxCollection<VersionRow>;
  milestones: TxCollection<MilestoneRow>;
  bugs: TxCollection<BugRow>;
  comments: TxCollection<CommentRow>;
  attachments: TxCollection<AttachmentRow>;
  keywords: TxCollection<KeywordRow>;
  bugKeywords: TxCollection<BugKeywordRow>;
  bugDependencies: TxCollection<DependencyRow>;
  bugCc: TxCollection<CcRow>;
  votes: TxCollection<VoteRow>;
  bugSeeAlso: TxCollection<SeeAlsoRow>;
  bugGroups: TxCollection<BugGroupRow>;
  flags: TxCollection<FlagRow>;
  activities: TxCollection<ActivityRow>;
  savedSearches: TxCollection<SavedSearchRow>;
  notifications: TxCollection<NotificationRow>;
};

type AppDb = {
  users: Collection<UserRow>;
  products: Collection<ProductRow>;
  components: Collection<ComponentRow>;
  versions: Collection<VersionRow>;
  milestones: Collection<MilestoneRow>;
  groups: Collection<GroupRow>;
  productGroups: Collection<ProductGroupRow>;
  groupMembers: Collection<GroupMemberRow>;
  bugGroups: Collection<BugGroupRow>;
  bugs: Collection<BugRow>;
  // NOT LISTED, and therefore unreachable from this file even though the
  // migration creates them: `bugSeeAlso`, `votes`, `watchers`. Their absence
  // here is why no procedure touches them -- SPEC.md describes voting and
  // watching, and neither is implemented. Adding the collection type is the
  // first step, not the feature.
  comments: Collection<CommentRow>;
  attachments: Collection<AttachmentRow>;
  keywords: Collection<KeywordRow>;
  bugKeywords: Collection<BugKeywordRow>;
  bugDependencies: Collection<DependencyRow>;
  bugCc: Collection<CcRow>;
  votes: Collection<VoteRow>;
  watchers: Collection<WatcherRow>;
  bugSeeAlso: Collection<SeeAlsoRow>;
  flagTypes: Collection<FlagTypeRow>;
  flags: Collection<FlagRow>;
  activities: Collection<ActivityRow>;
  savedSearches: Collection<SavedSearchRow>;
  notifications: Collection<NotificationRow>;
  transaction<R>(
    callback: (tx: TxDb) => Promise<R>,
    options?: { isolationLevel?: "serializable" },
  ): Promise<DbResult<R>>;
};

// This is only a structural view over the migration-installed handle. It does
// not register or declare a schema; the generated runtime descriptor remains
// the sole schema authority.
const db = (env as unknown as { db: AppDb }).db;

type PlatformUser = {
  id: string;
  email: string | null;
  name: string | null;
  avatar: string | null;
  emailVerified: boolean;
  scopes: string[];
};

type AuthNamespace = {
  getUser(): PlatformUser | null;
  requireUser(): PlatformUser;
};

type NativeStorage = {
  put(bucket: string, key: string, bytesBase64: string, contentType?: string): Promise<string>;
  get(bucket: string, key: string): Promise<string>;
  delete(bucket: string, key: string): Promise<string>;
};

type NativeKv = {
  get(key: string): Promise<string | null>;
  set(key: string, value: string, opts?: { ttlMs?: number }): Promise<unknown>;
  delete(key: string): Promise<{ deleted: boolean }>;
};

const ATTACHMENT_BUCKET = "issue-tracker-attachments";
const MAX_ATTACHMENT_BYTES = 512 * 1024;
const OPEN_STATUSES: readonly BugStatus[] = [
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
];
const NULLABLE_BUG_TEXT_FIELDS = new Set([
  "versionId",
  "milestoneId",
  "resolution",
  "assigneeId",
  "qaContactId",
  "duplicateOfId",
  "whiteboard",
  "url",
]);

function httpError(status: number, code: string, message: string): Error {
  return Object.assign(new Error(message), { status, code });
}

function invalid(message: string): never {
  throw httpError(400, "INVALID_ARGUMENT", message);
}

function notFound(noun: string): never {
  throw httpError(404, "NOT_FOUND", `${noun} not found`);
}

function conflict(message: string): never {
  throw httpError(409, "CONFLICT", message);
}

function forbidden(message = "You do not have access to this product"): never {
  throw httpError(403, "FORBIDDEN", message);
}

function must<T>(result: DbResult<T>): T {
  if (result.error) throw result.error;
  return result.data;
}

function requireNonEmpty(value: string, field: string): string {
  const clean = String(value ?? "").trim();
  if (!clean) invalid(`${field} is required`);
  return clean;
}

function requireId(value: string, field: string): string {
  const clean = requireNonEmpty(value, field);
  if (!/^[a-z][a-z0-9]*_[A-Za-z0-9]{22}$/u.test(clean)) {
    invalid(`${field} must be a typed id`);
  }
  return clean;
}

function clampLimit(value: number | undefined, fallback = 100): number {
  if (value === undefined) return fallback;
  if (!Number.isInteger(value) || value < 1) invalid("limit must be a positive integer");
  return Math.min(value, 500);
}

function clampOffset(value: number | undefined): number {
  if (value === undefined) return 0;
  if (!Number.isInteger(value) || value < 0) invalid("offset must be a non-negative integer");
  if (value > 10_000) invalid("offset cannot exceed 10000");
  return value;
}

const DATABASE_PAGE_SIZE = 500;

async function readAll<Row>(
  collection: Collection<Row>,
  filter: DbFilter = {},
): Promise<Row[]> {
  const rows: Row[] = [];
  let cursor: string | undefined;
  for (;;) {
    let pageQuery = collection.find(filter).sort({ id: 1 }).limit(DATABASE_PAGE_SIZE);
    if (cursor) pageQuery = pageQuery.after(cursor);
    const page = must(await pageQuery);
    rows.push(...page);
    if (page.length < DATABASE_PAGE_SIZE) return rows;
    cursor = (page[page.length - 1] as Row & { id: string }).id;
  }
}

async function readAllTx<Row>(
  collection: TxCollection<Row>,
  filter: DbFilter = {},
): Promise<Row[]> {
  const rows: Row[] = [];
  let cursor: string | undefined;
  for (;;) {
    let pageQuery = collection.find(filter).sort({ id: 1 }).limit(DATABASE_PAGE_SIZE);
    if (cursor) pageQuery = pageQuery.after(cursor);
    const page = await pageQuery;
    rows.push(...page);
    if (page.length < DATABASE_PAGE_SIZE) return rows;
    cursor = (page[page.length - 1] as Row & { id: string }).id;
  }
}

function chunks<T>(values: readonly T[], size = 100): T[][] {
  const pages: T[][] = [];
  for (let index = 0; index < values.length; index += size) {
    pages.push(values.slice(index, index + size));
  }
  return pages;
}

async function readByIds<Row>(
  collection: Collection<Row>,
  ids: readonly string[],
  extraFilter: DbFilter = {},
): Promise<Row[]> {
  if (ids.length === 0) return [];
  return (
    await Promise.all(
      chunks([...new Set(ids)]).map((batch) =>
        readAll(collection, {
          $and: [{ id: { $in: batch } }, extraFilter],
        }),
      ),
    )
  ).flat();
}

function impossibleFilter(): DbFilter {
  return { id: "__no_matching_typed_id__" };
}

function authNamespace(): AuthNamespace {
  const auth = (env as unknown as { auth?: AuthNamespace }).auth;
  if (!auth) {
    throw httpError(401, "UNAUTHENTICATED", "Authentication required");
  }
  return auth;
}

function requireIdentity(): PlatformUser {
  // The gateway policy is fail-closed, but this in-handler check is still
  // required because the local Vite runtime does not enforce gateway policy.
  return authNamespace().requireUser();
}

function identityEmail(identity: PlatformUser): string {
  if (!identity.email) {
    throw httpError(
      403,
      "EMAIL_SCOPE_REQUIRED",
      "This app requires the email identity scope",
    );
  }
  return identity.email;
}

function optionalIdentity(): PlatformUser | null {
  const auth = (env as unknown as { auth?: AuthNamespace }).auth;
  return auth?.getUser() ?? null;
}

function storage(): NativeStorage {
  const value = (env as unknown as { storage?: NativeStorage }).storage;
  if (!value) throw new Error("env.storage is not available");
  return value;
}

function kv(): NativeKv | null {
  return (env as unknown as { kv?: NativeKv }).kv ?? null;
}

function profileHandle(identity: PlatformUser): string {
  const local = identityEmail(identity).split("@", 1)[0]
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/gu, "-")
    .replace(/^[-.]+|[-.]+$/gu, "") || "user";
  const suffix = identity.id.replace(/[^a-zA-Z0-9]/gu, "").slice(-8).toLowerCase();
  return `${local.slice(0, 40)}-${suffix || "local"}`;
}

async function appUserForIdentity(identity: PlatformUser | null): Promise<UserRow | null> {
  if (!identity?.email) return null;
  return must(await db.users.get({ email: identity.email }));
}

/** Resolve the authenticated platform subject to the app-local users row. */
async function requireActor(): Promise<UserRow> {
  const identity = requireIdentity();
  const existing = await appUserForIdentity(identity);
  if (existing) {
    if (existing.isDisabled) forbidden("This issue-tracker account is disabled");
    return existing;
  }

  // The first account to exist is the admin, the way Bugzilla's installer
  // creates one at setup time. Without a bootstrap NOTHING can set isAdmin --
  // there is no RPC that grants it -- so `groups.create`, `products.restrict`
  // and every other admin path would be permanently unreachable and the access
  // model would be unconfigurable rather than merely strict.
  //
  // Racy by construction on a truly empty database (two simultaneous first
  // requests could both read zero). That is acceptable here and NOT a silent
  // hazard: the loser of the unique-email race is handled below, and the worst
  // case is two admins on a brand-new tracker, not an unguarded one.
  const isFirstAccount = (must(await db.users.count({})) ?? 0) === 0;

  const inserted = await db.users.insert({
    email: identityEmail(identity),
    handle: profileHandle(identity),
    name: identity.name || identityEmail(identity),
    timezone: "UTC",
    isAdmin: isFirstAccount,
    isDisabled: false,
  });
  if (!inserted.error) return inserted.data;

  // A concurrent first request may have won the unique-email insert race.
  const retry = await appUserForIdentity(identity);
  if (retry) return retry;
  throw inserted.error;
}

async function getRequired<Row>(collection: Collection<Row>, id: string, noun: string): Promise<Row> {
  const row = must(await collection.get(requireId(id, `${noun} id`)));
  if (!row) notFound(noun);
  return row;
}

async function getTxRequired<Row>(
  collection: TxCollection<Row>,
  id: string,
  noun: string,
): Promise<Row> {
  const row = await collection.get(requireId(id, `${noun} id`));
  if (!row) notFound(noun);
  return row;
}

async function canViewProduct(productId: string, user: UserRow | null): Promise<boolean> {
  if (user?.isDisabled) return false;
  if (user?.isAdmin) return true;
  const restrictions = await readAll(db.productGroups, { productId });
  if (restrictions.length === 0) return true;
  if (!user) return false;
  const memberships = await readAll(db.groupMembers, { userId: user.id });
  const groups = new Set(memberships.map((row) => row.groupId));
  return restrictions.some((row) => groups.has(row.groupId));
}

async function assertCanViewProduct(productId: string, user: UserRow | null): Promise<void> {
  if (!(await canViewProduct(productId, user))) forbidden();
}

async function visibleProductIds(
  identity: PlatformUser | null,
  includeInactive = false,
): Promise<Set<string>> {
  const user = await appUserForIdentity(identity);
  if (user?.isDisabled) return new Set();
  const products = await readAll(
    db.products,
    includeInactive ? {} : { isActive: true },
  );
  if (user?.isAdmin) return new Set(products.map((product) => product.id));

  const restrictions = await readAll(db.productGroups);
  const restrictedByProduct = new Map<string, Set<string>>();
  for (const row of restrictions) {
    const groups = restrictedByProduct.get(row.productId) ?? new Set<string>();
    groups.add(row.groupId);
    restrictedByProduct.set(row.productId, groups);
  }
  const memberships = user
    ? await readAll(db.groupMembers, { userId: user.id })
    : [];
  const myGroups = new Set(memberships.map((row) => row.groupId));

  return new Set(
    products
      .filter((product) => {
        const allowedGroups = restrictedByProduct.get(product.id);
        return !allowedGroups || [...allowedGroups].some((id) => myGroups.has(id));
      })
      .map((product) => product.id),
  );
}

// Bug-level security groups: Bugzilla's bug_group_map, the mechanism behind a
// confidential security bug inside an otherwise public product. Product-level
// visibility alone cannot express "this ONE bug is restricted".
//
// This existed as a table and nothing read it. `assertBugVisible` checked only
// the product, so every bugGroups row was decorative and a "restricted" bug was
// readable by anyone who could see its product.
async function canViewBug(bug: BugRow, user: UserRow | null): Promise<boolean> {
  if (!(await canViewProduct(bug.productId, user))) return false;
  const restrictions = await readAll(db.bugGroups, { bugId: bug.id });
  if (restrictions.length === 0) return true;
  if (user?.isAdmin) return true;
  if (!user) return false;
  const memberships = await readAll(db.groupMembers, { userId: user.id });
  const groups = new Set(memberships.map((row) => row.groupId));
  return restrictions.some((row) => groups.has(row.groupId));
}

async function assertBugVisible(bug: BugRow, identity: PlatformUser | null): Promise<void> {
  if (!(await canViewBug(bug, await appUserForIdentity(identity)))) forbidden();
}

/**
 * The write-path counterpart of `assertBugVisible`, for handlers that already
 * hold the actor.
 *
 * Every mutation that touches a bug used to call `assertCanViewProduct` -- the
 * PRODUCT check only -- so bug-level restriction guarded reads and nothing
 * else. Measured 2026-08-12: a second user got 403 from `bugs.get` on a
 * restricted bug and 200 from `comments.add` on the same bug, in the same
 * session. Commenting, resolving, reassigning, CC'ing, marking attachments
 * obsolete and deleting them were all reachable on a bug the caller could not
 * open.
 *
 * If you can't read it, you can't write it.
 */
async function assertBugAccessible(bug: BugRow, actor: UserRow | null): Promise<void> {
  if (!(await canViewBug(bug, actor))) forbidden();
}

/**
 * Bugs the user must not see because of a bug-level restriction.
 *
 * Returned as an exclusion list for the QUERY rather than applied by filtering
 * the result rows: post-filtering a page silently shrinks it, so a viewer with
 * a restricted bug in range gets a short page and the offsets stop meaning what
 * the caller thinks. `$nin` keeps limit/offset honest.
 */
async function hiddenBugIds(user: UserRow | null): Promise<string[]> {
  const restrictions = await readAll(db.bugGroups);
  if (restrictions.length === 0) return [];
  if (user?.isAdmin) return [];

  const groups = user
    ? new Set((await readAll(db.groupMembers, { userId: user.id })).map((row) => row.groupId))
    : new Set<string>();

  const byBug = new Map<string, string[]>();
  for (const row of restrictions) {
    byBug.set(row.bugId, [...(byBug.get(row.bugId) ?? []), row.groupId]);
  }
  return [...byBug.entries()]
    .filter(([, required]) => !required.some((groupId) => groups.has(groupId)))
    .map(([bugId]) => bugId);
}

async function validateProductChildren(
  productId: string,
  componentId: string,
  versionId?: string | null,
  milestoneId?: string | null,
): Promise<{ product: ProductRow; component: ComponentRow }> {
  const [product, component] = await Promise.all([
    getRequired(db.products, productId, "Product"),
    getRequired(db.components, componentId, "Component"),
  ]);
  if (component.productId !== product.id) invalid("componentId does not belong to productId");

  // `bugs.versionId` is NOT NULL: a Bugzilla bug is always filed against a
  // version. Omitting it used to fall through to the insert and surface as
  // HTTP 500 "internal error" -- the browser spec caught it by filing with the
  // form's "unspecified" version still selected. A missing version is the
  // caller's to fix, so it gets a 400 that says which field.
  if (!versionId) invalid("versionId is required");
  const version = await getRequired(db.versions, versionId, "Version");
  if (version.productId !== product.id) invalid("versionId does not belong to productId");
  if (milestoneId) {
    const milestone = await getRequired(db.milestones, milestoneId, "Milestone");
    if (milestone.productId !== product.id) invalid("milestoneId does not belong to productId");
  }
  return { product, component };
}

function bugState(row: BugRow): BugState {
  if (!isBugStatus(row.status)) invalid(`bug ${row.id} has an invalid stored status`);
  // The current DB update builder binds a nullable TEXT `$set: null` as an
  // empty text parameter. Keep the app's logical contract null-shaped at the
  // boundary (and in history/state checks) until that platform seam is fixed.
  const resolution = row.resolution ? row.resolution : null;
  if (resolution !== null && !isBugResolution(resolution)) {
    invalid(`bug ${row.id} has an invalid stored resolution`);
  }
  return { status: row.status, resolution };
}

function normalizeBugRow(row: BugRow): BugRow {
  const out = { ...row } as BugRow & Record<string, unknown>;
  for (const field of NULLABLE_BUG_TEXT_FIELDS) {
    if (out[field] === "" || out[field] === undefined) out[field] = null;
  }
  if (out.deadline === undefined) out.deadline = null;
  return out;
}

function logicalBugValue(field: string, value: unknown): unknown {
  return NULLABLE_BUG_TEXT_FIELDS.has(field) && (value === "" || value === undefined)
    ? null
    : value;
}

function asTrackedRecord(value: object): Record<string, string | number | boolean | null | undefined> {
  const record = {
    ...(value as Record<string, string | number | boolean | null | undefined>),
  };
  for (const field of NULLABLE_BUG_TEXT_FIELDS) {
    if (record[field] === "" || record[field] === undefined) record[field] = null;
  }
  return record;
}

async function insertActivities(
  activities: TxCollection<ActivityRow>,
  bugId: string,
  actorId: string,
  changes: readonly TrackedFieldChange[],
): Promise<void> {
  if (changes.length === 0) return;
  await activities.insertMany(
    changes.map((change) => ({
      bugId,
      actorId,
      fieldName: change.fieldName,
      ...(change.oldValue === null ? {} : { oldValue: change.oldValue }),
      ...(change.newValue === null ? {} : { newValue: change.newValue }),
    })),
  );
}

/** Write exactly one activity row for every actually changed tracked field. */
async function recordChanges(
  activities: TxCollection<ActivityRow>,
  bugId: string,
  actorId: string,
  before: object,
  after: object,
  fieldNames: readonly string[],
): Promise<void> {
  await insertActivities(
    activities,
    bugId,
    actorId,
    diffTrackedFields(asTrackedRecord(before), asTrackedRecord(after), fieldNames),
  );
}

async function recordRelatedChange(
  activities: TxCollection<ActivityRow>,
  bugId: string,
  actorId: string,
  fieldName: string,
  oldValue: string | null,
  newValue: string | null,
): Promise<void> {
  if (oldValue === newValue) return;
  await insertActivities(activities, bugId, actorId, [
    { fieldName, oldValue, newValue },
  ]);
}

async function updateBugWithHistory(
  id: string,
  actorId: string,
  makePatch: (before: BugRow) => DbPatch | Promise<DbPatch>,
  trackedFields: readonly string[],
): Promise<BugRow> {
  const result = await db.transaction(async (tx) => {
    const before = await getTxRequired(tx.bugs, id, "Bug");
    const candidate = await makePatch(before);
    const patch = Object.fromEntries(
      Object.entries(candidate).filter(
        ([field, value]) =>
          logicalBugValue(field, before[field as keyof BugRow]) !==
          logicalBugValue(field, value),
      ),
    );
    if (Object.keys(patch).length === 0) return normalizeBugRow(before);
    const after = await tx.bugs.update(id, patch);
    if (!after) notFound("Bug");
    await recordChanges(tx.activities, id, actorId, before, after, trackedFields);
    return normalizeBugRow(after);
  }, { isolationLevel: "serializable" });
  return must(result);
}

function rejectUnsupportedBugNullClears(
  before: BugRow,
  patch: Readonly<DbPatch>,
): void {
  const nullableForeignKeys = ["versionId", "milestoneId", "qaContactId"] as const;
  for (const field of nullableForeignKeys) {
    if (patch[field] === null && before[field]) {
      conflict(
        `clearing ${field} is unavailable until env.db supports SQL NULL updates`,
      );
    }
  }
  if (patch.deadline === null && before.deadline !== null && before.deadline !== undefined) {
    conflict("clearing deadline is unavailable until env.db supports SQL NULL updates");
  }
}

function dependencyEdges(rows: readonly DependencyRow[]): DirectedEdge[] {
  return rows.map((row) => ({ from: row.bugId, to: row.dependsOnId }));
}

function duplicateEdges(rows: readonly BugRow[]): DirectedEdge[] {
  return rows.flatMap((row) =>
    row.duplicateOfId ? [{ from: row.id, to: row.duplicateOfId }] : [],
  );
}

function safeFilename(filename: string): string {
  const cleaned = requireNonEmpty(filename, "filename")
    .replace(/[^A-Za-z0-9._-]+/gu, "-")
    .replace(/^[-.]+|-+$/gu, "")
    .slice(0, 120);
  return cleaned || "attachment.bin";
}

function attachmentStorageKey(bugId: string, filename: string): string {
  return `${bugId}/${crypto.randomUUID()}-${safeFilename(filename)}`;
}

function parseNativeJson<T>(raw: string): T {
  return JSON.parse(raw) as T;
}

function decodedBase64Size(value: string): number {
  const clean = value.replace(/\s/gu, "");
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/u.test(clean)) {
    invalid("contentBase64 is not valid base64");
  }
  if (clean.length === 0) invalid("attachment content cannot be empty");
  const padding = clean.endsWith("==") ? 2 : clean.endsWith("=") ? 1 : 0;
  return (clean.length / 4) * 3 - padding;
}

async function bugIdForFlag(flag: FlagRow): Promise<string> {
  if (flag.bugId) return flag.bugId;
  if (!flag.attachmentId) invalid("stored flag has no target");
  return (await getRequired(db.attachments, flag.attachmentId, "Attachment")).bugId;
}

function notificationCacheKey(userId: string): string {
  return `issue-tracker:unread:${userId}`;
}

async function setUnreadCache(userId: string, count: number): Promise<void> {
  const store = kv();
  if (!store) return;
  try {
    await store.set(notificationCacheKey(userId), JSON.stringify(count), { ttlMs: 30_000 });
  } catch {
    // Cache-aside only: a KV outage must not turn a committed DB write
    // into a failed RPC response.
  }
}

// ---------------------------------------------------------------------------
// Bugs
// ---------------------------------------------------------------------------

type CreateBugInput = {
  productId: string;
  componentId: string;
  summary: string;
  description: string;
  versionId?: string | null;
  milestoneId?: string | null;
  severity?: BugSeverity;
  priority?: BugPriority;
  assigneeId?: string | null;
  qaContactId?: string | null;
  whiteboard?: string | null;
  opSys?: string;
  platform?: string;
  url?: string | null;
  confirmed?: boolean;
  deadline?: number | null;
};

const BUG_CREATION_FIELDS = [
  "productId",
  "componentId",
  "versionId",
  "milestoneId",
  "summary",
  "status",
  "severity",
  "priority",
  "reporterId",
  "assigneeId",
  "qaContactId",
  "whiteboard",
  "opSys",
  "platform",
  "url",
  "isConfirmed",
  "voteCount",
  "commentCount",
  "deadline",
] as const;

export const createBug = mutation(
  async ({
    productId,
    componentId,
    summary,
    description,
    versionId,
    milestoneId,
    severity = "normal",
    priority = "P3",
    assigneeId,
    qaContactId,
    whiteboard,
    opSys = "Unspecified",
    platform = "Unspecified",
    url,
    confirmed,
    deadline,
  }: CreateBugInput) => {
    const actor = await requireActor();
    if (!BUG_SEVERITIES.includes(severity)) invalid("invalid severity");
    if (!BUG_PRIORITIES.includes(priority)) invalid("invalid priority");
    const structure = await validateProductChildren(
      requireId(productId, "productId"),
      requireId(componentId, "componentId"),
      versionId,
      milestoneId,
    );
    await assertCanViewProduct(structure.product.id, actor);
    if (!structure.product.isActive || !structure.component.isActive) {
      conflict("New bugs require an active product and component");
    }

    const isConfirmed = confirmed === true || !structure.product.allowsUnconfirmed;
    const status: BugStatus = isConfirmed ? "CONFIRMED" : "UNCONFIRMED";
    const cleanSummary = requireNonEmpty(summary, "summary").slice(0, 500);
    const cleanDescription = requireNonEmpty(description, "description");
    const result = await db.transaction(async (tx) => {
      const bug = await tx.bugs.insert({
        productId: structure.product.id,
        componentId: structure.component.id,
        ...(versionId ? { versionId } : {}),
        ...(milestoneId ? { milestoneId } : {}),
        summary: cleanSummary,
        status,
        severity,
        priority,
        reporterId: actor.id,
        ...(assigneeId ?? structure.component.defaultAssigneeId
          ? { assigneeId: assigneeId ?? structure.component.defaultAssigneeId }
          : {}),
        ...(qaContactId ?? structure.component.defaultQaContactId
          ? { qaContactId: qaContactId ?? structure.component.defaultQaContactId }
          : {}),
        ...(whiteboard ? { whiteboard } : {}),
        opSys,
        platform,
        ...(url ? { url } : {}),
        isConfirmed,
        voteCount: 0,
        commentCount: 1,
        ...(deadline === null || deadline === undefined ? {} : { deadline }),
      });
      await tx.comments.insert({
        bugId: bug.id,
        authorId: actor.id,
        body: cleanDescription,
        commentNumber: 0,
        isPrivate: false,
        workTimeMinutes: 0,
      });
      await recordChanges(
        tx.activities,
        bug.id,
        actor.id,
        {},
        bug,
        BUG_CREATION_FIELDS,
      );
      return bug;
    });
    return must(result);
  },
  { id: "bugs.create" },
);

export const getBug = query(
  async ({ id }: { id: string }) => {
    const storedBug = await getRequired(db.bugs, id, "Bug");
    await assertBugVisible(storedBug, optionalIdentity());
    const bug = normalizeBugRow(storedBug);
    const [activityRows, product, component] = await Promise.all([
      readAll(db.activities, { bugId: bug.id }),
      db.products.get(bug.productId),
      db.components.get(bug.componentId),
    ]);
    return {
      bug,
      product: must(product),
      component: must(component),
      activities: activityRows
        .sort((left, right) => left.created_at - right.created_at)
        .map((activity) => ({
          fieldName: activity.fieldName,
          oldValue: activity.oldValue ?? null,
          newValue: activity.newValue ?? null,
          changedAt: activity.created_at,
        })),
    };
  },
  { id: "bugs.get" },
);

type BugSearchInput = {
  productId?: string;
  componentId?: string;
  versionId?: string;
  milestoneId?: string;
  status?: BugStatus | BugStatus[];
  resolution?: BugResolution | BugResolution[] | null;
  severity?: BugSeverity | BugSeverity[];
  priority?: BugPriority | BugPriority[];
  assigneeId?: string | null;
  reporterId?: string;
  qaContactId?: string | null;
  isConfirmed?: boolean;
  text?: string;
  limit?: number;
  offset?: number;
  sortBy?: "created_at" | "updated_at" | "priority" | "severity" | "status";
  sortDirection?: 1 | -1;
};

function inFilter<T>(value: T | T[]): T | { $in: T[] } {
  if (!Array.isArray(value)) return value;
  if (value.length === 0) return { $in: ["__no_matching_value__" as T] };
  if (value.length > 100) invalid("filter lists accept at most 100 values");
  return { $in: value };
}

async function searchBugsInternal(
  input: BugSearchInput,
  identity: PlatformUser | null,
  extraFilter?: DbFilter,
): Promise<BugRow[]> {
  const visible = await visibleProductIds(identity);
  if (visible.size === 0) return [];
  if (input.productId && !visible.has(input.productId)) return [];

  const visibleIds = [...visible];
  const productVisibilityFilter: DbFilter = input.productId
    ? { productId: input.productId }
    : {
        $or: chunks(visibleIds).map((ids) => ({ productId: { $in: ids } })),
      };
  const base: DbFilter = {
    ...productVisibilityFilter,
    ...(input.componentId ? { componentId: input.componentId } : {}),
    ...(input.versionId ? { versionId: input.versionId } : {}),
    ...(input.milestoneId ? { milestoneId: input.milestoneId } : {}),
    ...(input.status ? { status: inFilter(input.status) } : {}),
    ...(input.resolution !== undefined && input.resolution !== null
      ? { resolution: inFilter(input.resolution) }
      : {}),
    ...(input.severity ? { severity: inFilter(input.severity) } : {}),
    ...(input.priority ? { priority: inFilter(input.priority) } : {}),
    ...(input.assigneeId !== undefined && input.assigneeId !== null
      ? { assigneeId: input.assigneeId }
      : {}),
    ...(input.reporterId ? { reporterId: input.reporterId } : {}),
    ...(input.qaContactId !== undefined && input.qaContactId !== null
      ? { qaContactId: input.qaContactId }
      : {}),
    ...(input.isConfirmed !== undefined ? { isConfirmed: input.isConfirmed } : {}),
  };
  const clauses: DbFilter[] = [base];
  if (input.resolution === null) {
    clauses.push({ $or: [{ resolution: null }, { resolution: "" }] });
  }
  if (input.assigneeId === null) {
    clauses.push({ $or: [{ assigneeId: null }, { assigneeId: "" }] });
  }
  if (input.qaContactId === null) {
    clauses.push({ $or: [{ qaContactId: null }, { qaContactId: "" }] });
  }
  if (input.text?.trim()) {
    const pattern = `%${input.text.trim()}%`;
    clauses.push({
      $or: [
        { summary: { $ilike: pattern } },
        { whiteboard: { $ilike: pattern } },
        { url: { $ilike: pattern } },
      ],
    });
  }
  if (extraFilter) clauses.push(extraFilter);

  // Bug-level restrictions are applied to every search, not only to bugs.get.
  // Enforcing on the detail route alone would keep a restricted bug's summary,
  // status and assignee listed on the bug list and in reports -- which is most
  // of what a confidential bug is trying not to leak.
  // Chunked for the same reason every `$in` in this file is: the list is
  // unbounded (one entry per restricted bug the viewer cannot see) and each
  // entry becomes a bind parameter, so one `NOT IN` would grow without limit.
  // `$nin` chunks cleanly where `$in` would not -- excluding A and excluding B
  // is the AND of the two, whereas including A or B is the OR.
  const hidden = await hiddenBugIds(await appUserForIdentity(identity));
  for (const page of chunks(hidden)) clauses.push({ id: { $nin: page } });

  const filter = clauses.length === 1 ? clauses[0] : { $and: clauses };
  const sortBy = input.sortBy ?? "updated_at";
  const direction = input.sortDirection ?? -1;
  return must(
    await db.bugs
      .find(filter)
      .sort({ [sortBy]: direction })
      .skip(clampOffset(input.offset))
      .limit(clampLimit(input.limit)),
  ).map(normalizeBugRow);
}

export const searchBugs = query(
  async ({
    productId,
    componentId,
    versionId,
    milestoneId,
    status,
    resolution,
    severity,
    priority,
    assigneeId,
    reporterId,
    qaContactId,
    isConfirmed,
    text,
    limit,
    offset,
    sortBy,
    sortDirection,
  }: BugSearchInput) =>
    searchBugsInternal(
      {
        productId,
        componentId,
        versionId,
        milestoneId,
        status,
        resolution,
        severity,
        priority,
        assigneeId,
        reporterId,
        qaContactId,
        isConfirmed,
        text,
        limit,
        offset,
        sortBy,
        sortDirection,
      },
      optionalIdentity(),
    ),
  { id: "bugs.search" },
);

type GeneralBugPatch = {
  summary?: string;
  versionId?: string | null;
  milestoneId?: string | null;
  qaContactId?: string | null;
  whiteboard?: string | null;
  opSys?: string;
  platform?: string;
  url?: string | null;
  deadline?: number | null;
};

const GENERAL_BUG_FIELDS = [
  "summary",
  "versionId",
  "milestoneId",
  "qaContactId",
  "whiteboard",
  "opSys",
  "platform",
  "url",
  "deadline",
] as const;

export const updateBug = mutation(
  async ({ id, changes }: { id: string; changes: GeneralBugPatch }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    const keys = Object.keys(changes);
    if (keys.length === 0) invalid("changes must contain at least one field");
    if (keys.some((key) => !(GENERAL_BUG_FIELDS as readonly string[]).includes(key))) {
      invalid("changes contains a field with a dedicated mutation");
    }
    if (changes.summary !== undefined) {
      changes = { ...changes, summary: requireNonEmpty(changes.summary, "summary") };
    }
    if (changes.versionId) {
      const version = await getRequired(db.versions, changes.versionId, "Version");
      if (version.productId !== current.productId) invalid("versionId belongs to another product");
    }
    if (changes.milestoneId) {
      const milestone = await getRequired(db.milestones, changes.milestoneId, "Milestone");
      if (milestone.productId !== current.productId) invalid("milestoneId belongs to another product");
    }
    return updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        rejectUnsupportedBugNullClears(before, changes);
        return { ...changes };
      },
      GENERAL_BUG_FIELDS,
    );
  },
  { id: "bugs.update" },
);

export const changeBugStatus = mutation(
  async ({
    id,
    status,
    resolution,
  }: {
    id: string;
    status: BugStatus;
    resolution?: BugResolution | null;
  }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    return updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        const next = transitionBugState(bugState(before), { status, resolution });
        return {
          status: next.status,
          resolution: next.resolution,
          isConfirmed: next.status !== "UNCONFIRMED",
          ...(next.resolution !== "DUPLICATE" ? { duplicateOfId: null } : {}),
        };
      },
      ["status", "resolution", "isConfirmed", "duplicateOfId"],
    );
  },
  { id: "bugs.changeStatus" },
);

export const resolveBug = mutation(
  async ({ id, resolution }: { id: string; resolution: Exclude<BugResolution, "DUPLICATE"> }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    const updated = await updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        const next = resolveBugState(bugState(before), resolution);
        return {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: null,
          isConfirmed: true,
          // Stamped here, and deliberately NOT in trackedFields below: it is
          // derived metadata, not a field a user edited, so it does not belong
          // in the bug's visible history.
          resolvedAt: portableTimestamp(Date.now()),
        };
      },
      ["status", "resolution", "duplicateOfId", "isConfirmed"],
    );
    await notifyBugChange(
      updated,
      actor.id,
      `${updated.summary} was resolved as ${resolution}`,
    );
    return updated;
  },
  { id: "bugs.resolve" },
);

export const reopenBug = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    return updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        const next = reopenBugState(bugState(before));
        return {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: null,
          isConfirmed: true,
          // Cleared on reopen for the same reason it is set on resolve: a
          // reopened bug is not resolved, and leaving a stale stamp would make
          // reports.timeToResolve count it as still-closed.
          resolvedAt: null,
        };
      },
      ["status", "resolution", "duplicateOfId", "isConfirmed"],
    );
  },
  { id: "bugs.reopen" },
);

export const markBugDuplicate = mutation(
  async ({ id, duplicateOfId }: { id: string; duplicateOfId: string }) => {
    const actor = await requireActor();
    const sourceId = requireId(id, "id");
    const targetId = requireId(duplicateOfId, "duplicateOfId");
    if (sourceId === targetId) invalid("a bug cannot duplicate itself");
    const [source, target] = await Promise.all([
      getRequired(db.bugs, sourceId, "Bug"),
      getRequired(db.bugs, targetId, "Duplicate target"),
    ]);
    await assertBugAccessible(source, actor);
    await assertBugAccessible(target, actor);

    const result = await db.transaction(
      async (tx) => {
        const before = await getTxRequired(tx.bugs, source.id, "Bug");
        await getTxRequired(tx.bugs, target.id, "Duplicate target");
        const allBugs = await readAllTx(tx.bugs, { duplicateOfId: { $ne: null } });
        if (wouldCreateDirectedCycle(duplicateEdges(allBugs), source.id, target.id)) {
          conflict("duplicate relationship would create a cycle");
        }
        const next = markDuplicateBugState(bugState(before));
        const after = await tx.bugs.update(source.id, {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: target.id,
          isConfirmed: true,
          // A duplicate is a resolution too, so it is stamped like the others.
          // Missing it here would silently exclude every duplicate from
          // reports.timeToResolve.
          resolvedAt: portableTimestamp(Date.now()),
        });
        if (!after) notFound("Bug");
        await recordChanges(
          tx.activities,
          source.id,
          actor.id,
          before,
          after,
          ["status", "resolution", "duplicateOfId", "isConfirmed"],
        );
        return normalizeBugRow(after);
      },
      { isolationLevel: "serializable" },
    );
    return must(result);
  },
  { id: "bugs.markDuplicate" },
);

export const reassignBug = mutation(
  async ({ id, assigneeId }: { id: string; assigneeId: string | null }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    if (assigneeId) await getRequired(db.users, assigneeId, "Assignee");
    return updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        if (assigneeId === null && before.assigneeId) {
          conflict("clearing assigneeId is unavailable until env.db supports SQL NULL updates");
        }
        return assigneeId !== null ? { assigneeId } : {};
      },
      ["assigneeId"],
    );
  },
  { id: "bugs.reassign" },
);

export const setBugSeverity = mutation(
  async ({ id, severity }: { id: string; severity: BugSeverity }) => {
    const actor = await requireActor();
    if (!BUG_SEVERITIES.includes(severity)) invalid("invalid severity");
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    return updateBugWithHistory(id, actor.id, () => ({ severity }), ["severity"]);
  },
  { id: "bugs.setSeverity" },
);

export const setBugPriority = mutation(
  async ({ id, priority }: { id: string; priority: BugPriority }) => {
    const actor = await requireActor();
    if (!BUG_PRIORITIES.includes(priority)) invalid("invalid priority");
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    return updateBugWithHistory(id, actor.id, () => ({ priority }), ["priority"]);
  },
  { id: "bugs.setPriority" },
);

export const moveBug = mutation(
  async ({ id, productId, componentId }: { id: string; productId: string; componentId: string }) => {
    const actor = await requireActor();
    const current = await getRequired(db.bugs, id, "Bug");
    await assertBugAccessible(current, actor);
    const structure = await validateProductChildren(productId, componentId);
    await assertCanViewProduct(structure.product.id, actor);
    return updateBugWithHistory(
      id,
      actor.id,
      (before) => {
        if (before.versionId || before.milestoneId) {
          conflict(
            "moving a versioned or milestone-assigned bug is unavailable until env.db supports SQL NULL updates",
          );
        }
        return {
          productId: structure.product.id,
          componentId: structure.component.id,
        };
      },
      ["productId", "componentId", "versionId", "milestoneId"],
    );
  },
  { id: "bugs.move" },
);

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

export const addComment = mutation(
  async ({
    bugId,
    body,
    isPrivate = false,
    workTimeMinutes = 0,
  }: {
    bugId: string;
    body: string;
    isPrivate?: boolean;
    workTimeMinutes?: number;
  }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);
    if (!Number.isFinite(workTimeMinutes) || workTimeMinutes < 0) {
      invalid("workTimeMinutes must be non-negative");
    }
    const cleanBody = requireNonEmpty(body, "body");

    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.bugs, bugId, "Bug");
      const after = await tx.bugs.update(bugId, {
        commentCount: { $inc: 1 },
      });
      if (!after) notFound("Bug");
      const commentNumber = after.commentCount - 1;
      const comment = await tx.comments.insert({
        bugId,
        authorId: actor.id,
        body: cleanBody,
        commentNumber,
        isPrivate,
        workTimeMinutes,
      });
      await recordChanges(
        tx.activities,
        bugId,
        actor.id,
        before,
        after,
        ["commentCount"],
      );
      return comment;
    }, { isolationLevel: "serializable" });
    const comment = must(result);
    // A private comment's BODY must not travel in a notification -- the
    // recipients of a fanout are not the same set as the people allowed to
    // read a private comment, so only the fact of a new comment goes out.
    await notifyBugChange(
      bug,
      actor.id,
      `New comment on ${bug.summary}`,
      isPrivate ? undefined : cleanBody.slice(0, 200),
    );
    return comment;
  },
  { id: "comments.add" },
);

export const listComments = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = optionalIdentity();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugVisible(bug, identity);
    const user = await appUserForIdentity(identity);
    const filter: DbFilter = user?.isAdmin
      ? { bugId }
      : user
        ? {
            $and: [
              { bugId },
              { $or: [{ isPrivate: false }, { authorId: user.id }] },
            ],
          }
        : { bugId, isPrivate: false };
    return must(
      await db.comments.find(filter).sort({ commentNumber: 1 }).limit(500),
    );
  },
  { id: "comments.list" },
);

export const editComment = mutation(
  async ({ id, body }: { id: string; body: string }) => {
    const actor = await requireActor();
    const comment = await getRequired(db.comments, id, "Comment");
    const bug = await getRequired(db.bugs, comment.bugId, "Bug");
    await assertBugAccessible(bug, actor);
    if (!actor.isAdmin && comment.authorId !== actor.id) {
      forbidden("Only the comment author or an administrator can edit it");
    }
    const cleanBody = requireNonEmpty(body, "body");
    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.comments, id, "Comment");
      if (before.body === cleanBody) return before;
      const after = await tx.comments.update(id, { body: cleanBody });
      if (!after) notFound("Comment");
      await recordRelatedChange(
        tx.activities,
        before.bugId,
        actor.id,
        `comment.${before.commentNumber}.body`,
        "previous text",
        "edited",
      );
      return after;
    });
    return must(result);
  },
  { id: "comments.edit" },
);

export const setCommentPrivate = mutation(
  async ({ id, isPrivate }: { id: string; isPrivate: boolean }) => {
    const actor = await requireActor();
    const comment = await getRequired(db.comments, id, "Comment");
    const bug = await getRequired(db.bugs, comment.bugId, "Bug");
    await assertBugAccessible(bug, actor);
    if (!actor.isAdmin && comment.authorId !== actor.id) {
      forbidden("Only the comment author or an administrator can change privacy");
    }
    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.comments, id, "Comment");
      if (before.isPrivate === isPrivate) return before;
      const after = await tx.comments.update(id, { isPrivate });
      if (!after) notFound("Comment");
      await recordRelatedChange(
        tx.activities,
        before.bugId,
        actor.id,
        `comment.${before.commentNumber}.isPrivate`,
        String(before.isPrivate),
        String(after.isPrivate),
      );
      return after;
    });
    return must(result);
  },
  { id: "comments.setPrivate" },
);

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

export const uploadAttachment = mutation(
  async ({
    bugId,
    filename,
    contentBase64,
    contentType = "application/octet-stream",
    description,
    isPatch = false,
  }: {
    bugId: string;
    filename: string;
    contentBase64: string;
    contentType?: string;
    description?: string | null;
    isPatch?: boolean;
  }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);
    const cleanFilename = safeFilename(filename);
    const cleanType = requireNonEmpty(contentType, "contentType").slice(0, 200);
    const cleanBase64 = contentBase64.replace(/\s/gu, "");
    const size = decodedBase64Size(cleanBase64);
    if (size > MAX_ATTACHMENT_BYTES) {
      throw httpError(
        413,
        "PAYLOAD_TOO_LARGE",
        `attachments are capped at ${MAX_ATTACHMENT_BYTES} bytes`,
      );
    }

    const storageKey = attachmentStorageKey(bugId, cleanFilename);
    const put = parseNativeJson<{ bucket: string; key: string; size: number }>(
      await storage().put(ATTACHMENT_BUCKET, storageKey, cleanBase64, cleanType),
    );
    const result = await db.transaction(async (tx) => {
      const attachment = await tx.attachments.insert({
        bugId,
        uploaderId: actor.id,
        filename: cleanFilename,
        contentType: cleanType,
        sizeBytes: put.size,
        storageKey,
        ...(description ? { description } : {}),
        isPatch,
        isObsolete: false,
      });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "attachment",
        null,
        `${attachment.id}:${cleanFilename}`,
      );
      return attachment;
    });
    if (result.error) {
      try {
        await storage().delete(ATTACHMENT_BUCKET, storageKey);
      } catch {
        // Preserve the database error. The storage key is content-isolated and
        // can be cleaned up safely by an operator if compensation also fails.
      }
      throw result.error;
    }
    return result.data;
  },
  { id: "attachments.upload" },
);

export const listAttachments = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugVisible(bug, identity);
    return must(
      await db.attachments.find({ bugId }).sort({ created_at: 1 }).limit(500),
    );
  },
  { id: "attachments.list" },
);

export const getAttachment = query(
  async ({ id }: { id: string }) => {
    const identity = requireIdentity();
    const attachment = await getRequired(db.attachments, id, "Attachment");
    const bug = await getRequired(db.bugs, attachment.bugId, "Bug");
    await assertBugVisible(bug, identity);
    const raw = await storage().get(ATTACHMENT_BUCKET, attachment.storageKey);
    if (raw === "null") notFound("Attachment content");
    const object = parseNativeJson<{
      bytesBase64: string;
      contentType: string | null;
      size: number;
    }>(raw);
    return {
      attachment,
      contentBase64: object.bytesBase64,
      contentType: object.contentType ?? attachment.contentType,
      sizeBytes: object.size,
    };
  },
  { id: "attachments.get" },
);

export const setAttachmentObsolete = mutation(
  async ({ id, isObsolete }: { id: string; isObsolete: boolean }) => {
    const actor = await requireActor();
    const attachment = await getRequired(db.attachments, id, "Attachment");
    const bug = await getRequired(db.bugs, attachment.bugId, "Bug");
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.attachments, id, "Attachment");
      const after = await tx.attachments.update(id, { isObsolete });
      if (!after) notFound("Attachment");
      await recordRelatedChange(
        tx.activities,
        before.bugId,
        actor.id,
        `attachment.${id}.isObsolete`,
        String(before.isObsolete),
        String(after.isObsolete),
      );
      return after;
    });
    return must(result);
  },
  { id: "attachments.setObsolete" },
);

export const deleteAttachment = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const attachment = await getRequired(db.attachments, id, "Attachment");
    const bug = await getRequired(db.bugs, attachment.bugId, "Bug");
    await assertBugAccessible(bug, actor);

    const raw = await storage().get(ATTACHMENT_BUCKET, attachment.storageKey);
    await storage().delete(ATTACHMENT_BUCKET, attachment.storageKey);
    const result = await db.transaction(async (tx) => {
      const attachmentFlags = await readAllTx(tx.flags, { attachmentId: id });
      for (const flag of attachmentFlags) {
        await tx.flags.delete(flag.id);
        await recordRelatedChange(
          tx.activities,
          attachment.bugId,
          actor.id,
          `flag.${flag.flagTypeId}`,
          flag.status,
          null,
        );
      }
      const deleted = await tx.attachments.delete(id);
      if (!deleted) notFound("Attachment");
      await recordRelatedChange(
        tx.activities,
        deleted.bugId,
        actor.id,
        "attachment",
        `${deleted.id}:${deleted.filename}`,
        null,
      );
      return deleted;
    });
    if (result.error) {
      // Storage and the database cannot share a transaction. Restore the bytes
      // if the relational half failed after object deletion.
      if (raw !== "null") {
        try {
          const object = parseNativeJson<{
            bytesBase64: string;
            contentType: string | null;
          }>(raw);
          await storage().put(
            ATTACHMENT_BUCKET,
            attachment.storageKey,
            object.bytesBase64,
            object.contentType ?? attachment.contentType,
          );
        } catch {
          // Preserve the transactional error; the failed compensation is an
          // operational orphan that should be surfaced by storage monitoring.
        }
      }
      throw result.error;
    }
    return { attachment: result.data, deleted: true };
  },
  { id: "attachments.delete" },
);

// ---------------------------------------------------------------------------
// Dependencies and duplicates
// ---------------------------------------------------------------------------

export const addDependency = mutation(
  async ({ bugId, dependsOnId }: { bugId: string; dependsOnId: string }) => {
    const actor = await requireActor();
    const sourceId = requireId(bugId, "bugId");
    const targetId = requireId(dependsOnId, "dependsOnId");
    if (sourceId === targetId) invalid("a bug cannot depend on itself");
    const [bug, dependency] = await Promise.all([
      getRequired(db.bugs, sourceId, "Bug"),
      getRequired(db.bugs, targetId, "Dependency"),
    ]);
    await assertBugAccessible(bug, actor);
    // The DEPENDENCY needs the same bug-level check as the bug being edited.
    // Checking only its product let a caller point an edge at a restricted bug
    // they cannot open -- and an edge is itself a disclosure: it confirms the
    // bug exists and names it in the tree. Missed by the earlier sweep because
    // this variable is `dependency`, not `bug`/`current`/`source`/`target`,
    // which is what a mechanical rename catches and a reading pass does not.
    await assertBugAccessible(dependency, actor);

    const result = await db.transaction(
      async (tx) => {
        const rows = await readAllTx(tx.bugDependencies);
        if (rows.some((row) => row.bugId === bug.id && row.dependsOnId === dependency.id)) {
          conflict("dependency already exists");
        }
        if (wouldCreateDirectedCycle(dependencyEdges(rows), bug.id, dependency.id)) {
          conflict("dependency would create a cycle");
        }
        const inserted = await tx.bugDependencies.insert({
          bugId: bug.id,
          dependsOnId: dependency.id,
        });
        await recordRelatedChange(
          tx.activities,
          bug.id,
          actor.id,
          "dependsOn",
          null,
          dependency.id,
        );
        return inserted;
      },
      { isolationLevel: "serializable" },
    );
    return must(result);
  },
  { id: "deps.add" },
);

export const removeDependency = mutation(
  async ({ bugId, dependsOnId }: { bugId: string; dependsOnId: string }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.bugDependencies.get({ bugId, dependsOnId });
      if (!existing) notFound("Dependency");
      await tx.bugDependencies.deleteMany({ bugId, dependsOnId });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "dependsOn",
        dependsOnId,
        null,
      );
      return existing;
    });
    return must(result);
  },
  { id: "deps.remove" },
);

async function visibleGraphRows(identity: PlatformUser): Promise<{
  bugs: BugRow[];
  dependencies: DependencyRow[];
}> {
  const visible = await visibleProductIds(identity, true);
  const productBatches = chunks([...visible]);
  const [bugPages, dependencies] = await Promise.all([
    Promise.all(productBatches.map((ids) => readAll(db.bugs, { productId: { $in: ids } }))),
    readAll(db.bugDependencies),
  ]);
  // Bug-level restrictions apply to the graph too. Filtering on product
  // visibility alone put a restricted bug into deps.tree and deps.graph as a
  // named node -- summary included -- for anyone who could see its product.
  // The edge filter below then keeps only edges whose BOTH ends survive, so a
  // hidden bug also stops leaking through its neighbours.
  const hidden = new Set(await hiddenBugIds(await appUserForIdentity(identity)));
  const bugRows = bugPages.flat().filter((bug) => !hidden.has(bug.id));
  const ids = new Set(bugRows.map((bug) => bug.id));
  return {
    bugs: bugRows,
    dependencies: dependencies.filter(
      (edge) => ids.has(edge.bugId) && ids.has(edge.dependsOnId),
    ),
  };
}

const MAX_DEPENDENCY_TREE_NODES = 2_000;

export const dependencyGraph = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const all = await visibleGraphRows(identity);
    const byId = new Map(all.bugs.map((bug) => [bug.id, bug]));
    if (!byId.has(bugId)) notFound("Bug");
    const component = new Set(
      weaklyConnectedComponent(dependencyEdges(all.dependencies), bugId),
    );
    return {
      nodes: [...component]
        .map((id) => byId.get(id))
        .filter((bug): bug is BugRow => bug !== undefined)
        .map(normalizeBugRow),
      edges: all.dependencies.filter(
        (edge) => component.has(edge.bugId) && component.has(edge.dependsOnId),
      ),
    };
  },
  { id: "deps.graph" },
);

type DependencyTreeNode = {
  bug: BugRow;
  dependencies: DependencyTreeNode[];
  cycle?: true;
};

export const dependencyTree = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const all = await visibleGraphRows(identity);
    const byId = new Map(all.bugs.map((bug) => [bug.id, bug]));
    if (!byId.has(bugId)) notFound("Bug");
    const children = new Map<string, string[]>();
    for (const edge of all.dependencies) {
      const values = children.get(edge.bugId) ?? [];
      values.push(edge.dependsOnId);
      children.set(edge.bugId, values);
    }
    let expanded = 0;
    const build = (id: string, path: ReadonlySet<string>): DependencyTreeNode => {
      expanded += 1;
      if (expanded > MAX_DEPENDENCY_TREE_NODES) {
        conflict(`dependency tree exceeds ${MAX_DEPENDENCY_TREE_NODES} expanded nodes`);
      }
      const storedBug = byId.get(id);
      if (!storedBug) notFound("Dependency bug");
      const bug = normalizeBugRow(storedBug);
      if (path.has(id)) return { bug, dependencies: [], cycle: true };
      const nextPath = new Set(path).add(id);
      return {
        bug,
        dependencies: (children.get(id) ?? []).map((child) => build(child, nextPath)),
      };
    };
    return build(bugId, new Set());
  },
  { id: "deps.tree" },
);

export const listDuplicates = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const visible = await visibleProductIds(identity, true);
    const bugs = (
      await Promise.all(
        chunks([...visible]).map((ids) => readAll(db.bugs, { productId: { $in: ids } })),
      )
    ).flat();
    if (!bugs.some((bug) => bug.id === bugId)) notFound("Bug");
    const ids = new Set(weaklyConnectedComponent(duplicateEdges(bugs), bugId));
    return bugs.filter((bug) => ids.has(bug.id)).map(normalizeBugRow);
  },
  { id: "dupes.list" },
);

// ---------------------------------------------------------------------------
// Keywords, flags, and CC
// ---------------------------------------------------------------------------

export const listKeywords = query(
  async ({}: EmptyInput) => {
    requireIdentity();
    return must(await db.keywords.find({}).sort({ name: 1 }).limit(500));
  },
  { id: "keywords.list" },
);

export const createKeyword = mutation(
  async ({ name, description }: { name: string; description?: string | null }) => {
    await requireActor();
    const cleanName = requireNonEmpty(name, "name").toLowerCase();
    const existing = must(await db.keywords.get({ name: cleanName }));
    if (existing) conflict("keyword already exists");
    return must(
      await db.keywords.insert({
        name: cleanName,
        ...(description ? { description } : {}),
      }),
    );
  },
  { id: "keywords.create" },
);

export const attachKeyword = mutation(
  async ({ bugId, keywordId }: { bugId: string; keywordId: string }) => {
    const actor = await requireActor();
    const [bug, keyword] = await Promise.all([
      getRequired(db.bugs, bugId, "Bug"),
      getRequired(db.keywords, keywordId, "Keyword"),
    ]);
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.bugKeywords.get({ bugId, keywordId });
      if (existing) return existing;
      const row = await tx.bugKeywords.insert({ bugId, keywordId });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "keywords",
        null,
        keyword.name,
      );
      return row;
    });
    return must(result);
  },
  { id: "keywords.attach" },
);

export const detachKeyword = mutation(
  async ({ bugId, keywordId }: { bugId: string; keywordId: string }) => {
    const actor = await requireActor();
    const [bug, keyword] = await Promise.all([
      getRequired(db.bugs, bugId, "Bug"),
      getRequired(db.keywords, keywordId, "Keyword"),
    ]);
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.bugKeywords.get({ bugId, keywordId });
      if (!existing) notFound("Bug keyword");
      await tx.bugKeywords.deleteMany({ bugId, keywordId });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "keywords",
        keyword.name,
        null,
      );
      return existing;
    });
    return must(result);
  },
  { id: "keywords.detach" },
);

export const setFlag = mutation(
  async ({
    flagTypeId,
    bugId,
    attachmentId,
    requesteeId,
    status,
  }: {
    flagTypeId: string;
    bugId?: string | null;
    attachmentId?: string | null;
    requesteeId?: string | null;
    status: "+" | "-" | "?";
  }) => {
    const actor = await requireActor();
    if (!["+", "-", "?"].includes(status)) invalid("flag status must be +, -, or ?");
    const flagType = await getRequired(db.flagTypes, flagTypeId, "Flag type");
    if (flagType.targetType !== "bug" && flagType.targetType !== "attachment") {
      invalid("flag type has an invalid targetType");
    }
    const target = assertFlagTarget(flagType.targetType, { bugId, attachmentId });
    const targetBugId = target.bugId
      ? target.bugId
      : (await getRequired(db.attachments, target.attachmentId!, "Attachment")).bugId;
    const bug = await getRequired(db.bugs, targetBugId, "Bug");
    await assertBugAccessible(bug, actor);
    if (flagType.productId && flagType.productId !== bug.productId) {
      invalid("flag type does not apply to the target product");
    }
    if (status === "?" && !flagType.isRequestable) {
      invalid("this flag type is not requestable");
    }
    if (requesteeId) await getRequired(db.users, requesteeId, "Requestee");

    const targetFilter: DbFilter = target.bugId
      ? { flagTypeId, bugId: target.bugId }
      : { flagTypeId, attachmentId: target.attachmentId };
    const result = await db.transaction(async (tx) => {
      const existing = !flagType.isMultiplicable
        ? await tx.flags.get(targetFilter)
        : null;
      if (existing && !requesteeId && existing.requesteeId) {
        conflict(
          "clearing requesteeId is unavailable until env.db supports SQL NULL updates",
        );
      }
      const flag = existing
        ? await tx.flags.update(existing.id, {
            status,
            setterId: actor.id,
            ...(requesteeId ? { requesteeId } : {}),
          })
        : await tx.flags.insert({
            flagTypeId,
            ...(target.bugId ? { bugId: target.bugId } : {}),
            ...(target.attachmentId ? { attachmentId: target.attachmentId } : {}),
            setterId: actor.id,
            ...(requesteeId ? { requesteeId } : {}),
            status,
          });
      if (!flag) notFound("Flag");
      await recordRelatedChange(
        tx.activities,
        targetBugId,
        actor.id,
        `flag.${flagType.name}`,
        existing?.status ?? null,
        status,
      );
      await recordRelatedChange(
        tx.activities,
        targetBugId,
        actor.id,
        `flag.${flagType.name}.requesteeId`,
        existing?.requesteeId ?? null,
        flag.requesteeId ?? null,
      );
      return flag;
    }, { isolationLevel: "serializable" });
    return must(result);
  },
  { id: "flags.set" },
);

export const clearFlag = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const flag = await getRequired(db.flags, id, "Flag");
    const [bugId, flagType] = await Promise.all([
      bugIdForFlag(flag),
      getRequired(db.flagTypes, flag.flagTypeId, "Flag type"),
    ]);
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const deleted = await tx.flags.delete(id);
      if (!deleted) notFound("Flag");
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        `flag.${flagType.name}`,
        deleted.status,
        null,
      );
      return deleted;
    });
    return must(result);
  },
  { id: "flags.clear" },
);

export const listFlagRequests = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return { setByMe: [], requestedOfMe: [] };
    const [setByMe, requestedOfMe, types] = await Promise.all([
      db.flags.find({ setterId: user.id, status: "?" }).sort({ created_at: -1 }),
      db.flags.find({ requesteeId: user.id, status: "?" }).sort({ created_at: -1 }),
      db.flagTypes.find({}),
    ]);
    const byId = new Map(must(types).map((type) => [type.id, type]));
    const hydrate = (flag: FlagRow) => ({ flag, flagType: byId.get(flag.flagTypeId) ?? null });
    return {
      setByMe: must(setByMe).map(hydrate),
      requestedOfMe: must(requestedOfMe).map(hydrate),
    };
  },
  { id: "flags.listRequests" },
);

// The flags currently on a bug, and on each of its attachments.
//
// WHY THIS EXISTS. Without it the only way a client could know a bug's flags
// was to replay the `activities` log and reconstruct them, which is wrong for
// any multiplicable flag type (several live flags of one type collapse to the
// last write) and leaves `flags.clear` unusable: clearing needs a flag id, and
// the id was only ever returned by `flags.set`, so a flag set in an earlier
// session could never be cleared at all. Reconstructing state from a history
// log is not a substitute for reading it.
export const listFlags = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugVisible(bug, identity);

    const attachments = await readAll(db.attachments, { bugId });
    const attachmentIds = new Set(attachments.map((row) => row.id));

    // Attachment flags carry no bugId, so they are found through the bug's
    // attachments rather than in one query.
    const [onBug, types] = await Promise.all([
      readAll(db.flags, { bugId }),
      readAll(db.flagTypes, {}),
    ]);
    const onAttachments = (
      await Promise.all(
        attachments.map((attachment) =>
          readAll(db.flags, { attachmentId: attachment.id }),
        ),
      )
    ).flat();

    const byType = new Map(types.map((type) => [type.id, type]));
    const users = await readByIds(db.users, [
      ...onBug.map((flag) => flag.setterId),
      ...onBug.map((flag) => flag.requesteeId),
      ...onAttachments.map((flag) => flag.setterId),
      ...onAttachments.map((flag) => flag.requesteeId),
    ].filter((id): id is string => Boolean(id)));
    const byUser = new Map(users.map((user) => [user.id, user]));

    const hydrate = (flag: FlagRow) => ({
      flag,
      flagType: byType.get(flag.flagTypeId) ?? null,
      setter: byUser.get(flag.setterId) ?? null,
      requestee: flag.requesteeId ? byUser.get(flag.requesteeId) ?? null : null,
    });

    return {
      onBug: onBug.map(hydrate),
      onAttachments: onAttachments
        .filter((flag) => flag.attachmentId && attachmentIds.has(flag.attachmentId))
        .map(hydrate),
    };
  },
  { id: "flags.list" },
);

export const addCc = mutation(
  async ({ bugId, userId }: { bugId: string; userId: string }) => {
    const actor = await requireActor();
    const [bug, user] = await Promise.all([
      getRequired(db.bugs, bugId, "Bug"),
      getRequired(db.users, userId, "User"),
    ]);
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.bugCc.get({ bugId, userId });
      if (existing) return existing;
      const row = await tx.bugCc.insert({ bugId, userId });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "cc",
        null,
        user.handle,
      );
      return row;
    });
    return must(result);
  },
  { id: "cc.add" },
);

export const removeCc = mutation(
  async ({ bugId, userId }: { bugId: string; userId: string }) => {
    const actor = await requireActor();
    const [bug, user] = await Promise.all([
      getRequired(db.bugs, bugId, "Bug"),
      getRequired(db.users, userId, "User"),
    ]);
    await assertBugAccessible(bug, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.bugCc.get({ bugId, userId });
      if (!existing) notFound("CC entry");
      await tx.bugCc.deleteMany({ bugId, userId });
      await recordRelatedChange(
        tx.activities,
        bugId,
        actor.id,
        "cc",
        user.handle,
        null,
      );
      return existing;
    });
    return must(result);
  },
  { id: "cc.remove" },
);

export const listCc = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugVisible(bug, identity);
    const rows = (await readAll(db.bugCc, { bugId })).sort(
      (left, right) => left.created_at - right.created_at,
    );
    if (rows.length === 0) return [];
    const users = await readByIds(db.users, rows.map((row) => row.userId));
    const byId = new Map(users.map((user) => [user.id, user]));
    return rows.map((row) => ({ ...row, user: byId.get(row.userId) ?? null }));
  },
  { id: "cc.list" },
);

// The bugs the caller is CC'd on -- the reverse of `cc.list`, and the query
// behind the dashboard's "CC'd to me" section.
//
// The schema was already indexed for this direction (`bug_cc_user_idx` on
// bugCc.userId) but no procedure read it, so the dashboard had an index and no
// way to reach it. Visibility is re-applied here rather than trusted from the
// CC row: being CC'd on a bug does not by itself grant access to a product the
// viewer can no longer see.
export const listMyCc = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];

    const rows = await readAll(db.bugCc, { userId: user.id });
    if (rows.length === 0) return [];

    const bugs = await readByIds(db.bugs, rows.map((row) => row.bugId));
    const visible = await visibleProductIds(identity);
    return bugs
      .filter((bug) => visible.has(bug.productId))
      .map(normalizeBugRow)
      .sort((left, right) => right.updated_at - left.updated_at);
  },
  { id: "cc.listMine" },
);

// ---------------------------------------------------------------------------
// Products and administration
// ---------------------------------------------------------------------------

export const listProducts = query(
  async ({
    classification,
    includeInactive = false,
  }: {
    classification?: string;
    includeInactive?: boolean;
  }) => {
    const identity = optionalIdentity();
    const user = await appUserForIdentity(identity);
    const maySeeInactive = includeInactive && user?.isAdmin === true;
    const visible = await visibleProductIds(identity, maySeeInactive);
    if (visible.size === 0) return [];
    const rows = await readByIds(db.products, [...visible], {
      ...(classification ? { classification } : {}),
      ...(maySeeInactive ? {} : { isActive: true }),
    });
    return rows.sort((left, right) => left.name.localeCompare(right.name));
  },
  { id: "products.list" },
);

export const getProduct = query(
  async ({ id }: { id: string }) => {
    const identity = requireIdentity();
    const product = await getRequired(db.products, id, "Product");
    await assertCanViewProduct(product.id, await appUserForIdentity(identity));
    const [components, versions, milestones, flagTypes] = await Promise.all([
      db.components.find({ productId: product.id }).sort({ name: 1 }),
      db.versions.find({ productId: product.id }).sort({ sortKey: 1 }),
      db.milestones.find({ productId: product.id }).sort({ sortKey: 1 }),
      db.flagTypes.find({
        $or: [{ productId: product.id }, { productId: null }],
      }),
    ]);
    return {
      product,
      components: must(components),
      versions: must(versions),
      milestones: must(milestones),
      flagTypes: must(flagTypes),
    };
  },
  { id: "products.get" },
);

export const createProduct = mutation(
  async ({
    name,
    description,
    classification = "Unclassified",
    defaultMilestone,
    allowsUnconfirmed = true,
    isActive = true,
  }: {
    name: string;
    description?: string | null;
    classification?: string;
    defaultMilestone?: string | null;
    allowsUnconfirmed?: boolean;
    isActive?: boolean;
  }) => {
    await requireActor();
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.products.get({ name: cleanName }))) conflict("product name already exists");
    return must(
      await db.products.insert({
        name: cleanName,
        ...(description ? { description } : {}),
        classification: requireNonEmpty(classification, "classification"),
        ...(defaultMilestone ? { defaultMilestone } : {}),
        allowsUnconfirmed,
        isActive,
      }),
    );
  },
  { id: "products.create" },
);

type ProductPatch = {
  name?: string;
  description?: string | null;
  classification?: string;
  defaultMilestone?: string | null;
  allowsUnconfirmed?: boolean;
  isActive?: boolean;
  votesPerUser?: number;
  maxVotesPerBug?: number;
  votesToConfirm?: number;
};

export const updateProduct = mutation(
  async ({ id, changes }: { id: string; changes: ProductPatch }) => {
    const actor = await requireActor();
    const product = await getRequired(db.products, id, "Product");
    await assertCanViewProduct(product.id, actor);
    const allowed = [
      "name",
      "description",
      "classification",
      "defaultMilestone",
      "allowsUnconfirmed",
      "isActive",
      // The voting limits are editable, or voting can never be turned on:
      // every product is created with all three at 0, which means disabled.
      "votesPerUser",
      "maxVotesPerBug",
      "votesToConfirm",
    ];
    if (Object.keys(changes).length === 0) invalid("changes must not be empty");
    if (Object.keys(changes).some((key) => !allowed.includes(key))) invalid("unsupported product field");
    if (changes.name !== undefined) {
      changes = { ...changes, name: requireNonEmpty(changes.name, "name") };
      const duplicate = must(await db.products.get({ name: changes.name }));
      if (duplicate && duplicate.id !== id) conflict("product name already exists");
    }
    if (changes.classification !== undefined) {
      changes = {
        ...changes,
        classification: requireNonEmpty(changes.classification, "classification"),
      };
    }
    const updated = must(await db.products.update(id, { ...changes }));
    if (!updated) notFound("Product");
    return updated;
  },
  { id: "products.update" },
);

export const listComponents = query(
  async ({ productId, includeInactive = false }: { productId: string; includeInactive?: boolean }) => {
    const identity = requireIdentity();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, await appUserForIdentity(identity));
    return must(
      await db.components
        .find({ productId, ...(includeInactive ? {} : { isActive: true }) })
        .sort({ name: 1 })
        .limit(500),
    );
  },
  { id: "components.list" },
);

export const createComponent = mutation(
  async ({
    productId,
    name,
    description,
    defaultAssigneeId,
    defaultQaContactId,
    isActive = true,
  }: {
    productId: string;
    name: string;
    description?: string | null;
    defaultAssigneeId?: string | null;
    defaultQaContactId?: string | null;
    isActive?: boolean;
  }) => {
    const actor = await requireActor();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, actor);
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.components.get({ productId, name: cleanName }))) {
      conflict("component name already exists in this product");
    }
    // `defaultAssigneeId` is NOT NULL in the schema, because a Bugzilla
    // component must have an initial owner -- that is what `bugs.create` falls
    // back to when no assignee is given. Omitting it therefore cannot mean
    // "leave it empty"; it means "the person creating the component owns it",
    // which is the useful default and the only one that satisfies the column.
    //
    // Before this, omitting it let a NULL reach the insert and the NOT NULL
    // violation surfaced as HTTP 500 "internal error" -- a caller-fixable input
    // problem reported as a server fault, with nothing in the body to act on.
    const assigneeId = defaultAssigneeId ?? actor.id;
    await getRequired(db.users, assigneeId, "Default assignee");
    if (defaultQaContactId) await getRequired(db.users, defaultQaContactId, "Default QA contact");
    return must(
      await db.components.insert({
        productId,
        name: cleanName,
        ...(description ? { description } : {}),
        defaultAssigneeId: assigneeId,
        ...(defaultQaContactId ? { defaultQaContactId } : {}),
        isActive,
      }),
    );
  },
  { id: "components.create" },
);

type ComponentPatch = {
  name?: string;
  description?: string | null;
  defaultAssigneeId?: string | null;
  defaultQaContactId?: string | null;
  isActive?: boolean;
};

export const updateComponent = mutation(
  async ({ id, changes }: { id: string; changes: ComponentPatch }) => {
    const actor = await requireActor();
    const component = await getRequired(db.components, id, "Component");
    await assertCanViewProduct(component.productId, actor);
    const allowed = [
      "name",
      "description",
      "defaultAssigneeId",
      "defaultQaContactId",
      "isActive",
    ];
    if (Object.keys(changes).length === 0) invalid("changes must not be empty");
    if (Object.keys(changes).some((key) => !allowed.includes(key))) invalid("unsupported component field");
    if (changes.name !== undefined) {
      changes = { ...changes, name: requireNonEmpty(changes.name, "name") };
      const duplicate = must(
        await db.components.get({ productId: component.productId, name: changes.name }),
      );
      if (duplicate && duplicate.id !== id) conflict("component name already exists in this product");
    }
    if (changes.defaultAssigneeId) {
      await getRequired(db.users, changes.defaultAssigneeId, "Default assignee");
    }
    if (changes.defaultQaContactId) {
      await getRequired(db.users, changes.defaultQaContactId, "Default QA contact");
    }
    if (changes.defaultAssigneeId === null && component.defaultAssigneeId) {
      conflict(
        "clearing defaultAssigneeId is unavailable until env.db supports SQL NULL updates",
      );
    }
    if (changes.defaultQaContactId === null && component.defaultQaContactId) {
      conflict(
        "clearing defaultQaContactId is unavailable until env.db supports SQL NULL updates",
      );
    }
    changes = Object.fromEntries(
      Object.entries(changes).filter(
        ([field, value]) =>
          !(
            value === null &&
            (field === "defaultAssigneeId" || field === "defaultQaContactId")
          ),
      ),
    ) as ComponentPatch;
    if (Object.keys(changes).length === 0) return component;
    const updated = must(await db.components.update(id, { ...changes }));
    if (!updated) notFound("Component");
    return updated;
  },
  { id: "components.update" },
);

export const listVersions = query(
  async ({ productId, includeInactive = false }: { productId: string; includeInactive?: boolean }) => {
    const identity = requireIdentity();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, await appUserForIdentity(identity));
    return must(
      await db.versions
        .find({ productId, ...(includeInactive ? {} : { isActive: true }) })
        .sort({ sortKey: 1 })
        .limit(500),
    );
  },
  { id: "versions.list" },
);

export const createVersion = mutation(
  async ({
    productId,
    name,
    sortKey = 0,
    isActive = true,
  }: {
    productId: string;
    name: string;
    sortKey?: number;
    isActive?: boolean;
  }) => {
    const actor = await requireActor();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, actor);
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.versions.get({ productId, name: cleanName }))) {
      conflict("version name already exists in this product");
    }
    if (!Number.isFinite(sortKey)) invalid("sortKey must be finite");
    return must(await db.versions.insert({ productId, name: cleanName, sortKey, isActive }));
  },
  { id: "versions.create" },
);

export const listMilestones = query(
  async ({ productId, includeInactive = false }: { productId: string; includeInactive?: boolean }) => {
    const identity = requireIdentity();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, await appUserForIdentity(identity));
    return must(
      await db.milestones
        .find({ productId, ...(includeInactive ? {} : { isActive: true }) })
        .sort({ sortKey: 1 })
        .limit(500),
    );
  },
  { id: "milestones.list" },
);

export const createMilestone = mutation(
  async ({
    productId,
    name,
    sortKey = 0,
    isActive = true,
  }: {
    productId: string;
    name: string;
    sortKey?: number;
    isActive?: boolean;
  }) => {
    const actor = await requireActor();
    const product = await getRequired(db.products, productId, "Product");
    await assertCanViewProduct(product.id, actor);
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.milestones.get({ productId, name: cleanName }))) {
      conflict("milestone name already exists in this product");
    }
    if (!Number.isFinite(sortKey)) invalid("sortKey must be finite");
    return must(await db.milestones.insert({ productId, name: cleanName, sortKey, isActive }));
  },
  { id: "milestones.create" },
);

// ---------------------------------------------------------------------------
// Structured search, QuickSearch, and saved searches
// ---------------------------------------------------------------------------

type SearchField =
  | "id"
  | "productId"
  | "componentId"
  | "versionId"
  | "milestoneId"
  | "summary"
  | "status"
  | "resolution"
  | "severity"
  | "priority"
  | "assigneeId"
  | "reporterId"
  | "qaContactId"
  | "whiteboard"
  | "opSys"
  | "platform"
  | "url"
  | "isConfirmed"
  | "deadline"
  | "created_at"
  | "updated_at";

type SearchOperator =
  | "eq"
  | "ne"
  | "in"
  | "notIn"
  | "contains"
  | "gt"
  | "gte"
  | "lt"
  | "lte"
  | "exists";

type AdvancedSearchNode =
  | { op: "and" | "or"; clauses: AdvancedSearchNode[] }
  | { op: "not"; clause: AdvancedSearchNode }
  | { field: SearchField; operator: SearchOperator; value?: unknown };

const SEARCH_FIELDS = new Set<SearchField>([
  "id",
  "productId",
  "componentId",
  "versionId",
  "milestoneId",
  "summary",
  "status",
  "resolution",
  "severity",
  "priority",
  "assigneeId",
  "reporterId",
  "qaContactId",
  "whiteboard",
  "opSys",
  "platform",
  "url",
  "isConfirmed",
  "deadline",
  "created_at",
  "updated_at",
]);

const STRING_SEARCH_FIELDS = new Set<SearchField>([
  "id",
  "productId",
  "componentId",
  "versionId",
  "milestoneId",
  "summary",
  "status",
  "resolution",
  "severity",
  "priority",
  "assigneeId",
  "reporterId",
  "qaContactId",
  "whiteboard",
  "opSys",
  "platform",
  "url",
]);

function validateEnumCondition(field: SearchField, value: unknown): void {
  const values = Array.isArray(value) ? value : [value];
  if (field === "status" && values.some((item) => !isBugStatus(item))) {
    invalid("advanced search contains an invalid status");
  }
  if (field === "resolution" && values.some((item) => item !== null && !isBugResolution(item))) {
    invalid("advanced search contains an invalid resolution");
  }
  if (
    field === "severity" &&
    values.some((item) => !BUG_SEVERITIES.includes(item as BugSeverity))
  ) {
    invalid("advanced search contains an invalid severity");
  }
  if (
    field === "priority" &&
    values.some((item) => !BUG_PRIORITIES.includes(item as BugPriority))
  ) {
    invalid("advanced search contains an invalid priority");
  }
}

function compileAdvancedSearch(
  node: AdvancedSearchNode,
  state = { count: 0 },
  depth = 0,
): DbFilter {
  if (!node || typeof node !== "object") invalid("search clause must be an object");
  if (depth > 8) invalid("advanced search nesting is too deep");
  state.count += 1;
  if (state.count > 100) invalid("advanced search has too many clauses");

  if ("op" in node) {
    if (node.op === "not") {
      if (!("clause" in node)) invalid("not requires one clause");
      return { $not: compileAdvancedSearch(node.clause, state, depth + 1) };
    }
    if (node.op !== "and" && node.op !== "or") invalid("invalid boolean operator");
    if (!("clauses" in node) || !Array.isArray(node.clauses) || node.clauses.length === 0) {
      invalid(`${node.op} requires at least one clause`);
    }
    return {
      [node.op === "and" ? "$and" : "$or"]: node.clauses.map((clause) =>
        compileAdvancedSearch(clause, state, depth + 1),
      ),
    };
  }

  if (!SEARCH_FIELDS.has(node.field)) invalid(`unsupported search field: ${String(node.field)}`);
  validateEnumCondition(node.field, node.value);
  switch (node.operator) {
    case "eq":
      if (node.value === null && NULLABLE_BUG_TEXT_FIELDS.has(node.field)) {
        return { $or: [{ [node.field]: null }, { [node.field]: "" }] };
      }
      return { [node.field]: node.value };
    case "ne":
      if (node.value === null && NULLABLE_BUG_TEXT_FIELDS.has(node.field)) {
        return {
          $and: [
            { [node.field]: { $ne: null } },
            { [node.field]: { $ne: "" } },
          ],
        };
      }
      return { [node.field]: { $ne: node.value } };
    case "in":
      if (!Array.isArray(node.value)) invalid("in requires an array value");
      if (node.value.length === 0) return impossibleFilter();
      if (node.value.length > 100) invalid("in accepts at most 100 values");
      if (node.value.includes(null)) {
        if (!NULLABLE_BUG_TEXT_FIELDS.has(node.field)) {
          invalid("null membership requires a nullable field");
        }
        const nonNull = node.value.filter((value) => value !== null);
        return {
          $or: [
            { [node.field]: null },
            { [node.field]: "" },
            ...(nonNull.length > 0 ? [{ [node.field]: { $in: nonNull } }] : []),
          ],
        };
      }
      return { [node.field]: { $in: node.value } };
    case "notIn":
      if (!Array.isArray(node.value)) invalid("notIn requires an array value");
      if (node.value.length === 0) return {};
      if (node.value.length > 100) invalid("notIn accepts at most 100 values");
      if (node.value.includes(null)) {
        if (!NULLABLE_BUG_TEXT_FIELDS.has(node.field)) {
          invalid("null membership requires a nullable field");
        }
        const nonNull = node.value.filter((value) => value !== null);
        return {
          $and: [
            { [node.field]: { $ne: null } },
            { [node.field]: { $ne: "" } },
            ...(nonNull.length > 0 ? [{ [node.field]: { $nin: nonNull } }] : []),
          ],
        };
      }
      return { [node.field]: { $nin: node.value } };
    case "contains":
      if (!STRING_SEARCH_FIELDS.has(node.field) || typeof node.value !== "string") {
        invalid("contains requires a string field and string value");
      }
      return { [node.field]: { $ilike: `%${node.value}%` } };
    case "gt":
    case "gte":
    case "lt":
    case "lte":
      if (typeof node.value !== "number" && typeof node.value !== "string") {
        invalid(`${node.operator} requires a string or number value`);
      }
      return { [node.field]: { [`$${node.operator}`]: node.value } };
    case "exists":
      if (typeof node.value !== "boolean") invalid("exists requires a boolean value");
      return { [node.field]: { $exists: node.value } };
    default:
      invalid(`unsupported search operator: ${String(node.operator)}`);
  }
}

export const structuredSearch = query(
  async ({
    where,
    limit,
    offset,
    sortBy,
    sortDirection,
  }: {
    where: AdvancedSearchNode;
    limit?: number;
    offset?: number;
    sortBy?: BugSearchInput["sortBy"];
    sortDirection?: 1 | -1;
  }) => {
    const identity = requireIdentity();
    return searchBugsInternal(
      { limit, offset, sortBy, sortDirection },
      identity,
      compileAdvancedSearch(where),
    );
  },
  { id: "search.query" },
);

async function quickClauseFilter(clause: QuickSearchClause): Promise<DbFilter> {
  switch (clause.field) {
    case "priority":
      return { priority: clause.value };
    case "status":
      return { status: clause.value };
    case "resolution":
      return { resolution: clause.value };
    case "severity":
      return { severity: clause.value };
    case "text": {
      const pattern = `%${clause.value}%`;
      return {
        $or: [
          { summary: { $ilike: pattern } },
          { whiteboard: { $ilike: pattern } },
          { url: { $ilike: pattern } },
        ],
      };
    }
    case "assignee": {
      const users = must(await db.users.find({ handle: clause.value }));
      return users.length > 0
        ? { assigneeId: { $in: users.map((user) => user.id) } }
        : impossibleFilter();
    }
    case "reporter": {
      const users = must(await db.users.find({ handle: clause.value }));
      return users.length > 0
        ? { reporterId: { $in: users.map((user) => user.id) } }
        : impossibleFilter();
    }
    case "component": {
      const rows = must(
        await db.components.find({ name: { $ilike: `%${clause.value}%` } }),
      );
      return rows.length > 0
        ? { componentId: { $in: rows.map((row) => row.id) } }
        : impossibleFilter();
    }
    case "product": {
      const rows = must(
        await db.products.find({ name: { $ilike: `%${clause.value}%` } }),
      );
      return rows.length > 0
        ? { productId: { $in: rows.map((row) => row.id) } }
        : impossibleFilter();
    }
  }
}

export const quickSearch = query(
  async ({
    text,
    limit,
    offset,
  }: {
    text: string;
    limit?: number;
    offset?: number;
  }) => {
    const identity = requireIdentity();
    const clauses = parseQuickSearch(text);
    const filters = await Promise.all(clauses.map(quickClauseFilter));
    const bugs = await searchBugsInternal(
      { limit, offset },
      identity,
      filters.length > 0 ? { $and: filters } : undefined,
    );
    return { clauses, bugs };
  },
  { id: "search.quick" },
);

export const listSavedSearches = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];
    return must(
      await db.savedSearches
        .find({ $or: [{ ownerId: user.id }, { isShared: true }] })
        .sort({ name: 1 })
        .limit(500),
    );
  },
  { id: "savedSearches.list" },
);

export const saveSavedSearch = mutation(
  async ({
    id,
    name,
    queryJson,
    isShared = false,
  }: {
    id?: string;
    name: string;
    queryJson: unknown;
    isShared?: boolean;
  }) => {
    const actor = await requireActor();
    const cleanName = requireNonEmpty(name, "name");
    let serialized: string;
    try {
      serialized = JSON.stringify(queryJson);
    } catch {
      invalid("queryJson must be JSON serializable");
    }
    if (serialized === undefined) invalid("queryJson must be JSON serializable");

    let row: SavedSearchRow;
    if (id) {
      const existing = must(await db.savedSearches.get({ id, ownerId: actor.id }));
      if (!existing) notFound("Saved search");
      const updated = must(
        await db.savedSearches.update({ id, ownerId: actor.id }, {
          name: cleanName,
          queryJson,
          isShared,
        }),
      );
      if (!updated) notFound("Saved search");
      row = updated;
    } else {
      const existing = must(
        await db.savedSearches.get({ ownerId: actor.id, name: cleanName }),
      );
      if (existing) conflict("a saved search with this name already exists");
      row = must(
        await db.savedSearches.insert({
          ownerId: actor.id,
          name: cleanName,
          queryJson,
          isShared,
        }),
      );
    }
    const store = kv();
    if (store) {
      try {
        await store.set(
          `issue-tracker:saved-search:${actor.id}:${row.id}`,
          serialized,
          { ttlMs: 5 * 60_000 },
        );
      } catch {
        // The database is authoritative; a cache outage must not make the
        // already-committed saved-search write appear to have failed.
      }
    }
    return row;
  },
  { id: "savedSearches.save" },
);

export const deleteSavedSearch = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const deleted = must(await db.savedSearches.delete({ id, ownerId: actor.id }));
    if (!deleted) notFound("Saved search");
    const store = kv();
    if (store) {
      try {
        await store.delete(`issue-tracker:saved-search:${actor.id}:${id}`);
      } catch {
        // Cache-aside invalidation is best effort. The deleted row remains the
        // source of truth and list calls never read this cache directly.
      }
    }
    return deleted;
  },
  { id: "savedSearches.delete" },
);

// ---------------------------------------------------------------------------
// Users and notifications
// ---------------------------------------------------------------------------

export const currentUser = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const profile = await appUserForIdentity(identity);
    if (profile) return { ...profile, isProvisioned: true as const };
    return {
      id: null,
      platformSubjectId: identity.id,
      email: identity.email,
      handle: identity.email ? profileHandle(identity) : null,
      name: identity.name || identity.email || "Anonymous profile",
      timezone: "UTC",
      isAdmin: false,
      isDisabled: false,
      isProvisioned: false as const,
    };
  },
  { id: "users.me" },
);

export const listUsers = query(
  async ({
    text,
    includeDisabled = false,
    limit,
  }: {
    text?: string;
    includeDisabled?: boolean;
    limit?: number;
  }) => {
    const identity = requireIdentity();
    const caller = await appUserForIdentity(identity);
    const filter: DbFilter = {
      ...(includeDisabled && caller?.isAdmin ? {} : { isDisabled: false }),
      ...(text?.trim()
        ? {
            $or: [
              { name: { $ilike: `%${text.trim()}%` } },
              { handle: { $ilike: `%${text.trim()}%` } },
              { email: { $ilike: `%${text.trim()}%` } },
            ],
          }
        : {}),
    };
    return must(
      await db.users.find(filter).sort({ name: 1 }).limit(clampLimit(limit, 100)),
    );
  },
  { id: "users.list" },
);

export const getUser = query(
  async ({ id }: { id: string }) => {
    requireIdentity();
    return getRequired(db.users, id, "User");
  },
  { id: "users.get" },
);

export const updateUserPrefs = mutation(
  async ({
    name,
    prefs,
    timezone,
  }: {
    name?: string;
    prefs?: Record<string, unknown> | null;
    timezone?: string;
  }) => {
    const actor = await requireActor();
    if (name === undefined && prefs === undefined && timezone === undefined) {
      invalid("at least one preference must be supplied");
    }
    const patch: DbPatch = {
      ...(name === undefined ? {} : { name: requireNonEmpty(name, "name") }),
      // Replaces the whole bag rather than merging: a merge would make it
      // impossible to delete a preference key through this endpoint.
      ...(prefs === undefined ? {} : { prefs }),
      ...(timezone === undefined
        ? {}
        : { timezone: requireNonEmpty(timezone, "timezone") }),
    };
    const updated = must(await db.users.update(actor.id, patch));
    if (!updated) notFound("User");
    return updated;
  },
  { id: "users.updatePrefs" },
);

export const listNotifications = query(
  async ({
    unreadOnly = false,
    limit,
    offset,
  }: {
    unreadOnly?: boolean;
    limit?: number;
    offset?: number;
  }) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];
    return must(
      await db.notifications
        .find({ userId: user.id, ...(unreadOnly ? { isRead: false } : {}) })
        .sort({ created_at: -1 })
        .skip(clampOffset(offset))
        .limit(clampLimit(limit, 100)),
    );
  },
  { id: "notifications.list" },
);

export const markNotificationRead = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const notification = must(
      await db.notifications.get({ id: requireId(id, "id"), userId: actor.id }),
    );
    if (!notification) notFound("Notification");
    const updated = notification.isRead
      ? notification
      : must(
          await db.notifications.update(
            { id: notification.id, userId: actor.id },
            { isRead: true },
          ),
        );
    if (!updated) notFound("Notification");
    const count = must(
      await db.notifications.count({ userId: actor.id, isRead: false }),
    );
    await setUnreadCache(actor.id, count);
    return updated;
  },
  { id: "notifications.markRead" },
);

export const unreadNotificationCount = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return { count: 0, cached: false };
    const store = kv();
    if (store) {
      try {
        const cached = await store.get(notificationCacheKey(user.id));
        if (cached !== null) {
          const value = JSON.parse(cached) as unknown;
          if (typeof value === "number" && Number.isFinite(value)) {
            return { count: value, cached: true };
          }
        }
      } catch {
        // A malformed/stale cache value or KV outage falls through to the
        // authoritative database count.
      }
    }
    const count = must(
      await db.notifications.count({ userId: user.id, isRead: false }),
    );
    return { count, cached: false };
  },
  { id: "notifications.unreadCount" },
);

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

type ReportInput = {
  productId?: string;
  days?: number;
};

function reportDays(value: number | undefined): number {
  if (value === undefined) return 30;
  if (!Number.isInteger(value) || value < 1 || value > 365) {
    invalid("days must be an integer from 1 through 365");
  }
  return value;
}

async function reportBugs(productId?: string): Promise<BugRow[]> {
  const identity = optionalIdentity();
  const visible = await visibleProductIds(identity);
  if (productId && !visible.has(productId)) return [];
  if (visible.size === 0) return [];

  const rows = productId
    ? await readAll(db.bugs, { productId })
    : (
        await Promise.all(
          chunks([...visible]).map((ids) => readAll(db.bugs, { productId: { $in: ids } })),
        )
      ).flat();

  // Bug-level restrictions apply to AGGREGATES too. This filtered on product
  // visibility only, so a bug restricted to a security group still landed in
  // reports.summary, byComponent, byAssignee and trend -- leaking its
  // existence, status, severity and assignee to everyone who could see the
  // product. reports.* are anon-accessible, so that audience was "anyone".
  //
  // A count is not a lesser disclosure than a row: "this product has 3 open
  // blockers" is most of what a confidential bug is trying not to say.
  const hidden = new Set(await hiddenBugIds(await appUserForIdentity(identity)));
  return hidden.size === 0 ? rows : rows.filter((row) => !hidden.has(row.id));
}

async function activityEventsForBugs(
  bugIds: readonly string[],
  filter: DbFilter,
): Promise<ActivityRow[]> {
  return (
    await Promise.all(
      chunks([...new Set(bugIds)]).map((ids) =>
        readAll(db.activities, {
          $and: [{ bugId: { $in: ids } }, filter],
        }),
      ),
    )
  ).flat();
}

function portableTimestamp(timestamp: number): string {
  return new Date(timestamp).toISOString().replace("T", " ");
}

function countsBy<Row>(rows: readonly Row[], key: (row: Row) => string): Record<string, number> {
  const out: Record<string, number> = {};
  for (const row of rows) {
    const value = key(row);
    out[value] = (out[value] ?? 0) + 1;
  }
  return out;
}

export const reportSummary = query(
  async ({ productId }: ReportInput) => {
    const all = await reportBugs(productId);
    const open = all.filter((bug) => OPEN_STATUSES.includes(bug.status as BugStatus));
    return {
      total: all.length,
      open: open.length,
      byStatus: countsBy(open, (bug) => bug.status),
      bySeverity: countsBy(open, (bug) => bug.severity),
      byPriority: countsBy(open, (bug) => bug.priority),
    };
  },
  { id: "reports.summary" },
);

export const reportByComponent = query(
  async ({ productId }: ReportInput) => {
    const open = (await reportBugs(productId)).filter((bug) =>
      OPEN_STATUSES.includes(bug.status as BugStatus),
    );
    if (open.length === 0) return [];
    const components = await readByIds(
      db.components,
      open.map((bug) => bug.componentId),
    );
    const byId = new Map(components.map((component) => [component.id, component]));
    const grouped = new Map<string, BugRow[]>();
    for (const bug of open) {
      const values = grouped.get(bug.componentId) ?? [];
      values.push(bug);
      grouped.set(bug.componentId, values);
    }
    return [...grouped.entries()]
      .map(([componentId, bugs]) => ({
        component: byId.get(componentId) ?? null,
        count: bugs.length,
        byStatus: countsBy(bugs, (bug) => bug.status),
      }))
      .sort((a, b) => b.count - a.count);
  },
  { id: "reports.byComponent" },
);

export const reportByAssignee = query(
  async ({ productId }: ReportInput) => {
    const open = (await reportBugs(productId)).filter((bug) =>
      OPEN_STATUSES.includes(bug.status as BugStatus),
    );
    const assigneeIds = [
      ...new Set(open.flatMap((bug) => (bug.assigneeId ? [bug.assigneeId] : []))),
    ];
    const users = await readByIds(db.users, assigneeIds);
    const byId = new Map(users.map((user) => [user.id, user]));
    const grouped = new Map<string, BugRow[]>();
    for (const bug of open) {
      const key = bug.assigneeId ?? "unassigned";
      const values = grouped.get(key) ?? [];
      values.push(bug);
      grouped.set(key, values);
    }
    return [...grouped.entries()]
      .map(([assigneeId, bugs]) => ({
        assignee: assigneeId === "unassigned"
          ? null
          : (() => {
              const user = byId.get(assigneeId);
              return user
                ? { id: user.id, handle: user.handle, name: user.name }
                : null;
            })(),
        count: bugs.length,
        byPriority: countsBy(bugs, (bug) => bug.priority),
      }))
      .sort((a, b) => b.count - a.count);
  },
  { id: "reports.byAssignee" },
);

function utcDay(timestamp: number): string {
  return new Date(timestamp).toISOString().slice(0, 10);
}

export const reportTrend = query(
  async ({ productId, days }: ReportInput) => {
    const windowDays = reportDays(days);
    const now = Date.now();
    const today = Date.UTC(
      new Date(now).getUTCFullYear(),
      new Date(now).getUTCMonth(),
      new Date(now).getUTCDate(),
    );
    const start = today - (windowDays - 1) * 86_400_000;
    const bugs = await reportBugs(productId);
    const activities = await activityEventsForBugs(
      bugs.map((bug) => bug.id),
      {
        fieldName: "status",
        newValue: "RESOLVED",
        created_at: { $gte: portableTimestamp(start) },
      },
    );
    const buckets = new Map<string, { date: string; created: number; resolved: number }>();
    for (let index = 0; index < windowDays; index += 1) {
      const date = utcDay(start + index * 86_400_000);
      buckets.set(date, { date, created: 0, resolved: 0 });
    }
    for (const bug of bugs) {
      if (bug.created_at < start) continue;
      const bucket = buckets.get(utcDay(bug.created_at));
      if (bucket) bucket.created += 1;
    }
    for (const activity of activities) {
      const bucket = buckets.get(utcDay(activity.created_at));
      if (bucket) bucket.resolved += 1;
    }
    return [...buckets.values()];
  },
  { id: "reports.trend" },
);

export const reportTimeToResolve = query(
  async ({ productId, days }: ReportInput) => {
    const windowDays = reportDays(days);
    const start = Date.now() - windowDays * 86_400_000;
    // Read the stamped column rather than replaying history. The previous
    // implementation scanned `activities` for fieldName="status",
    // newValue="RESOLVED" and took the earliest per bug -- a scan with string
    // matching, and env.db has no raw SQL to make it cheaper. `resolvedAt` is
    // maintained by bugs.resolve / bugs.markDuplicate / bugs.reopen and is
    // indexed (bugs_resolved_at_idx).
    //
    // The `typeof === "number"` test is load-bearing and not defensive noise:
    // clearing this column on reopen stores an EMPTY STRING rather than SQL
    // NULL (measured 2026-08-12 on the SQLite dev backend -- a reopened bug
    // reads back `resolvedAt: ""`). A `!= null` test would let that through
    // and `"" - created_at` is NaN, which would silently poison the average.
    const bugs = await reportBugs(productId);
    const durations = bugs
      .filter((bug) => typeof bug.resolvedAt === "number" && bug.resolvedAt >= start)
      .map((bug) => (bug.resolvedAt as number) - bug.created_at)
      .filter((duration) => duration >= 0)
      .sort((a, b) => a - b);
    if (durations.length === 0) {
      return { resolvedCount: 0, averageMs: null, medianMs: null };
    }
    const total = durations.reduce((sum, value) => sum + value, 0);
    const middle = Math.floor(durations.length / 2);
    const median = durations.length % 2 === 1
      ? durations[middle]
      : (durations[middle - 1] + durations[middle]) / 2;
    return {
      resolvedCount: durations.length,
      averageMs: total / durations.length,
      medianMs: median,
    };
  },
  { id: "reports.timeToResolve" },
);

// ---------------------------------------------------------------------------
// Groups and access restrictions
//
// These exist because the enforcement code did not: `productGroups` and
// `bugGroups` were both readable by the visibility helpers and writable by
// nothing, so no restriction could ever be created and every visibility branch
// was dead. A permission check that cannot be switched on is not a permission
// check -- it reads like protection in the source and denies nobody at runtime.
// ---------------------------------------------------------------------------

async function requireAdmin(): Promise<UserRow> {
  const actor = await requireActor();
  if (!actor.isAdmin) forbidden("Only a platform admin can administer groups");
  return actor;
}

export const createGroup = mutation(
  async ({ name, description }: { name: string; description?: string | null }) => {
    await requireAdmin();
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.groups.get({ name: cleanName }))) conflict("group name already exists");
    return must(
      await db.groups.insert({
        name: cleanName,
        ...(description ? { description } : {}),
        isBugGroup: true,
      }),
    );
  },
  { id: "groups.create" },
);

export const listGroups = query(
  async ({}: EmptyInput) => {
    await requireAdmin();
    return readAll(db.groups, {});
  },
  { id: "groups.list" },
);

export const addGroupMember = mutation(
  async ({ groupId, userId }: { groupId: string; userId: string }) => {
    await requireAdmin();
    await getRequired(db.groups, groupId, "Group");
    await getRequired(db.users, userId, "User");
    const existing = must(await db.groupMembers.get({ groupId, userId }));
    if (existing) return existing;
    return must(await db.groupMembers.insert({ groupId, userId }));
  },
  { id: "groups.addMember" },
);

export const removeGroupMember = mutation(
  async ({ groupId, userId }: { groupId: string; userId: string }) => {
    await requireAdmin();
    const existing = must(await db.groupMembers.get({ groupId, userId }));
    if (!existing) notFound("Group membership");
    must(await db.groupMembers.delete(existing.id));
    return { removed: true };
  },
  { id: "groups.removeMember" },
);

export const restrictProduct = mutation(
  async ({ productId, groupId }: { productId: string; groupId: string }) => {
    await requireAdmin();
    await getRequired(db.products, productId, "Product");
    await getRequired(db.groups, groupId, "Group");
    const existing = must(await db.productGroups.get({ productId, groupId }));
    if (existing) return existing;
    return must(await db.productGroups.insert({ productId, groupId }));
  },
  { id: "products.restrict" },
);

export const unrestrictProduct = mutation(
  async ({ productId, groupId }: { productId: string; groupId: string }) => {
    await requireAdmin();
    const existing = must(await db.productGroups.get({ productId, groupId }));
    if (!existing) notFound("Product restriction");
    must(await db.productGroups.delete(existing.id));
    return { removed: true };
  },
  { id: "products.unrestrict" },
);

export const restrictBug = mutation(
  async ({ bugId, groupId }: { bugId: string; groupId: string }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    if (!(await canViewBug(bug, actor))) forbidden();
    await getRequired(db.groups, groupId, "Group");

    // Bugzilla requires you to be IN a group to put a bug into it, and the
    // reason is not bureaucratic: without this an ordinary user could restrict
    // a bug to a group they are not in and lock themselves -- and everyone
    // else outside it -- out of a bug they could previously read.
    if (!actor.isAdmin) {
      const membership = must(await db.groupMembers.get({ groupId, userId: actor.id }));
      if (!membership) forbidden("You must belong to a group to restrict a bug to it");
    }

    // The restriction row and its history entry go in together: a restriction
    // with no audit trail is exactly the change someone later needs to explain.
    return must(
      await db.transaction(async (tx) => {
        const existing = await tx.bugGroups.get({ bugId, groupId });
        if (existing) return existing;
        const row = await tx.bugGroups.insert({ bugId, groupId });
        await recordRelatedChange(tx.activities, bugId, actor.id, "bug_group", null, groupId);
        return row;
      }),
    );
  },
  { id: "bugs.restrict" },
);

export const unrestrictBug = mutation(
  async ({ bugId, groupId }: { bugId: string; groupId: string }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    if (!(await canViewBug(bug, actor))) forbidden();
    return must(
      await db.transaction(async (tx) => {
        const existing = await tx.bugGroups.get({ bugId, groupId });
        if (!existing) notFound("Bug restriction");
        await tx.bugGroups.delete(existing.id);
        await recordRelatedChange(tx.activities, bugId, actor.id, "bug_group", groupId, null);
        return { removed: true };
      }),
    );
  },
  { id: "bugs.unrestrict" },
);

// ---------------------------------------------------------------------------
// Voting
//
// The `votes` table and the three product columns (votesPerUser,
// maxVotesPerBug, votesToConfirm) were added for this and then nothing used
// them: zero server references, so the schema described a feature the app did
// not have and a migration comment claimed it enabled "the classic
// votes-auto-confirm flow" that no code performed.
//
// Bugzilla's rules, which are the point of the three columns:
//   - a user spends at most `votesPerUser` votes across a product,
//   - at most `maxVotesPerBug` of them on any one bug,
//   - and when a bug reaches `votesToConfirm`, an UNCONFIRMED bug is confirmed.
// A product with the columns left at 0 has voting disabled, which is why 0 is
// the default rather than something permissive.
// ---------------------------------------------------------------------------

export const castVote = mutation(
  async ({ bugId, count }: { bugId: string; count: number }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);

    if (!Number.isInteger(count) || count < 0) invalid("count must be a non-negative integer");
    const product = await getRequired(db.products, bug.productId, "Product");
    if (product.votesPerUser <= 0) conflict("voting is disabled for this product");
    if (product.maxVotesPerBug > 0 && count > product.maxVotesPerBug) {
      invalid(`at most ${product.maxVotesPerBug} votes may be cast on one bug`);
    }

    return must(
      await db.transaction(async (tx) => {
        // The per-product budget counts this user's votes on OTHER bugs in the
        // same product, so replacing an existing vote frees its own allowance
        // rather than counting twice.
        const mine = await readAllTx(tx.votes, { userId: actor.id });
        const existing = mine.find((row) => row.bugId === bugId) ?? null;
        const bugIds = mine.map((row) => row.bugId).filter((id) => id !== bugId);
        const otherBugs: BugRow[] = [];
        for (const id of bugIds) {
          const row = await tx.bugs.get(id);
          if (row) otherBugs.push(row);
        }
        const spentElsewhere = otherBugs
          .filter((other) => other.productId === bug.productId)
          .reduce((total, other) => {
            const row = mine.find((entry) => entry.bugId === other.id);
            return total + (row?.count ?? 0);
          }, 0);
        if (spentElsewhere + count > product.votesPerUser) {
          conflict(
            `only ${product.votesPerUser} votes are available per user in this product ` +
              `(${spentElsewhere} already spent elsewhere)`,
          );
        }

        if (existing && count === 0) {
          await tx.votes.delete(existing.id);
        } else if (existing) {
          await tx.votes.update(existing.id, { count });
        } else if (count > 0) {
          await tx.votes.insert({ bugId, userId: actor.id, count });
        }

        // voteCount is SUM(count), not COUNT(*) -- a vote row carries a
        // quantity. Recomputed from the rows rather than incremented, so it
        // cannot drift away from them.
        const after = await readAllTx(tx.votes, { bugId });
        const total = after.reduce((sum, row) => sum + row.count, 0);

        const patch: DbPatch = { voteCount: total };
        // Bugzilla's auto-confirm: enough votes turn an UNCONFIRMED bug into a
        // CONFIRMED one. Only from UNCONFIRMED -- votes never move a bug that
        // is already resolved.
        const confirms =
          product.votesToConfirm > 0 &&
          total >= product.votesToConfirm &&
          bug.status === "UNCONFIRMED";
        if (confirms) {
          patch.status = "CONFIRMED";
          patch.isConfirmed = true;
        }
        const updated = await tx.bugs.update(bugId, patch);
        if (!updated) notFound("Bug");

        await recordRelatedChange(
          tx.activities,
          bugId,
          actor.id,
          "votes",
          String(bug.voteCount),
          String(total),
        );
        if (confirms) {
          await recordRelatedChange(
            tx.activities,
            bugId,
            actor.id,
            "status",
            "UNCONFIRMED",
            "CONFIRMED",
          );
        }
        return { bugId, count, voteCount: total, confirmed: confirms };
      }),
    );
  },
  { id: "votes.cast" },
);

export const listMyVotes = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];
    const rows = await readAll(db.votes, { userId: user.id });
    if (rows.length === 0) return [];
    const bugs = await readByIds(db.bugs, rows.map((row) => row.bugId));
    const hidden = new Set(await hiddenBugIds(user));
    const visible = await visibleProductIds(identity);
    const byId = new Map(bugs.map((bug) => [bug.id, bug]));
    return rows
      .filter((row) => {
        const bug = byId.get(row.bugId);
        return bug && !hidden.has(bug.id) && visible.has(bug.productId);
      })
      .map((row) => ({ ...row, bug: normalizeBugRow(byId.get(row.bugId)!) }));
  },
  { id: "votes.listMine" },
);

// ---------------------------------------------------------------------------
// Watching and notification fanout
//
// The `notifications` table had three READ procedures (list, markRead,
// unreadCount) and NO writer: nothing in the app ever inserted a row, so the
// inbox was permanently empty and the nav's unread badge could never appear.
// `watchers` was likewise a table with zero server references.
//
// Fanout runs AFTER the mutation's transaction commits, not inside it. That is
// a deliberate trade: a notification is a side effect, and losing one is much
// better than rolling back the bug change that caused it. It does mean a crash
// between commit and fanout drops the notification silently.
// ---------------------------------------------------------------------------

/**
 * Everyone who should hear about a change to this bug: assignee, reporter, QA
 * contact, the CC list, and the watchers of each of those -- minus the actor,
 * who already knows.
 *
 * Recipients are filtered by `canViewBug`. Notifying someone about a bug they
 * cannot open would leak its summary in the notification title, which is
 * exactly what a restricted bug is hiding.
 */
async function notifyBugChange(
  bug: BugRow,
  actorId: string,
  title: string,
  body?: string,
): Promise<void> {
  const direct = new Set<string>(
    [bug.assigneeId, bug.reporterId, bug.qaContactId].filter(
      (id): id is string => Boolean(id),
    ),
  );
  for (const row of await readAll(db.bugCc, { bugId: bug.id })) direct.add(row.userId);

  // Watchers of each interested party also hear about it -- Bugzilla's
  // "user watching", where a lead follows everything their reports touch.
  const watched = new Set(direct);
  for (const page of chunks([...direct])) {
    for (const row of await readAll(db.watchers, { watchedId: { $in: page } })) {
      watched.add(row.watcherId);
    }
  }
  watched.delete(actorId);
  if (watched.size === 0) return;

  const recipients = await readByIds(db.users, [...watched]);
  for (const user of recipients) {
    if (user.isDisabled) continue;
    if (!(await canViewBug(bug, user))) continue;
    const inserted = await db.notifications.insert({
      userId: user.id,
      bugId: bug.id,
      kind: "bug_changed",
      title,
      ...(body ? { body } : {}),
      isRead: false,
    });
    // A failed notification must not fail the mutation that already committed.
    if (inserted.error) {
      console.warn(`notification insert failed for ${user.id}: ${String(inserted.error)}`);
      continue;
    }
    // The unread count is cached in KV and written by notifications.markRead.
    // Inserting a row without refreshing it leaves anyone who has EVER marked
    // something read looking at a stale count -- their cache is warm, so the
    // authoritative fallback never runs. Recomputed rather than incremented so
    // it cannot drift from the rows.
    const fresh = must(await db.notifications.count({ userId: user.id, isRead: false }));
    await setUnreadCache(user.id, fresh);
  }
}

export const addWatcher = mutation(
  async ({ watchedId }: { watchedId: string }) => {
    const actor = await requireActor();
    const watched = await getRequired(db.users, watchedId, "User");
    if (watched.id === actor.id) invalid("you cannot watch yourself");
    const existing = must(await db.watchers.get({ watcherId: actor.id, watchedId: watched.id }));
    if (existing) return existing;
    return must(await db.watchers.insert({ watcherId: actor.id, watchedId: watched.id }));
  },
  { id: "watchers.add" },
);

export const removeWatcher = mutation(
  async ({ watchedId }: { watchedId: string }) => {
    const actor = await requireActor();
    const existing = must(await db.watchers.get({ watcherId: actor.id, watchedId }));
    if (!existing) notFound("Watch");
    must(await db.watchers.delete(existing.id));
    return { removed: true };
  },
  { id: "watchers.remove" },
);

export const listWatchers = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];
    const rows = await readAll(db.watchers, { watcherId: user.id });
    if (rows.length === 0) return [];
    const watched = await readByIds(db.users, rows.map((row) => row.watchedId));
    const byId = new Map(watched.map((entry) => [entry.id, entry]));
    return rows.map((row) => ({ ...row, watched: byId.get(row.watchedId) ?? null }));
  },
  { id: "watchers.list" },
);

// ---------------------------------------------------------------------------
// See Also
//
// Bugzilla's cross-tracker links: a bug in another system that is the same
// issue, or related to it. The table existed with zero server references, so
// the schema described the feature and nothing implemented it.
//
// Stored as a plain URL rather than a parsed reference: the whole point is
// pointing at trackers this app knows nothing about.
// ---------------------------------------------------------------------------

export const addSeeAlso = mutation(
  async ({ bugId, url }: { bugId: string; url: string }) => {
    const actor = await requireActor();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugAccessible(bug, actor);

    const clean = requireNonEmpty(url, "url");
    // Scheme-checked rather than accepted verbatim: this value is rendered as
    // a link, and `javascript:` in an href is script execution, not a link.
    let parsed: URL;
    try {
      parsed = new URL(clean);
    } catch {
      invalid("url must be absolute");
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      invalid("url must be http or https");
    }

    const existing = must(await db.bugSeeAlso.get({ bugId, url: clean }));
    if (existing) return existing;
    return must(
      await db.transaction(async (tx) => {
        const row = await tx.bugSeeAlso.insert({ bugId, url: clean });
        await recordRelatedChange(tx.activities, bugId, actor.id, "see_also", null, clean);
        return row;
      }),
    );
  },
  { id: "seeAlso.add" },
);

export const removeSeeAlso = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const row = must(await db.bugSeeAlso.get(requireId(id, "id")));
    if (!row) notFound("See Also link");
    const bug = await getRequired(db.bugs, row.bugId, "Bug");
    await assertBugAccessible(bug, actor);
    return must(
      await db.transaction(async (tx) => {
        await tx.bugSeeAlso.delete(row.id);
        await recordRelatedChange(tx.activities, row.bugId, actor.id, "see_also", row.url, null);
        return { removed: true };
      }),
    );
  },
  { id: "seeAlso.remove" },
);

export const listSeeAlso = query(
  async ({ bugId }: { bugId: string }) => {
    const identity = requireIdentity();
    const bug = await getRequired(db.bugs, bugId, "Bug");
    await assertBugVisible(bug, identity);
    return readAll(db.bugSeeAlso, { bugId });
  },
  { id: "seeAlso.list" },
);
