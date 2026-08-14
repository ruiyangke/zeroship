"use server";

// Bugzilla-faithful issue tracker RPC layer.
//
// The schema is deliberately NOT declared here. The committed migrations are
// folded by @zeroship/vite-plugin into generated/zeroship/schema.runtime.json,
// and the runtime installs the resulting collections on env.db before this
// module is evaluated.

import { env } from "zeroship";
import { mutation, query } from "@zeroship/rpc/server";
import {
  ISSUE_CREATION_FIELDS,
  diffTrackedFields,
  type TrackedFieldChange,
} from "./lib/changes";
import { assertFlagTarget } from "./lib/flags";
import {
  weaklyConnectedComponent,
  wouldCreateDirectedCycle,
  type DirectedEdge,
} from "./lib/graph";
import {
  ISSUE_KINDS,
  ISSUE_PRIORITIES,
  ISSUE_SEVERITIES,
  parseQuickSearch,
  type IssueKind,
  type IssuePriority,
  type IssueSeverity,
  type QuickSearchClause,
} from "./lib/quicksearch";
import {
  ISSUE_RESOLUTIONS,
  ISSUE_STATUSES,
  isIssueResolution,
  isIssueStatus,
  isOpenIssueStatus,
  markDuplicateIssueState,
  reopenIssueState,
  resolveIssueState,
  transitionIssueState,
  type IssueResolution,
  type IssueState,
  type IssueStatus,
} from "./lib/workflow";

import type {
  ActivityRow,
  AppDb,
  AttachmentRow,
  IssueGroupRow,
  IssueKeywordRow,
  IssueRow,
  CcRow,
  Collection,
  CommentRow,
  ComponentRow,
  DbFilter,
  DbPatch,
  DbResult,
  DependencyRow,
  EmptyInput,
  FlagRow,
  FlagTypeRow,
  GroupMemberRow,
  GroupRow,
  KeywordRow,
  MilestoneRow,
  NotificationRow,
  ProductGroupRow,
  ProductRow,
  ReadQuery,
  SavedSearchRow,
  SeeAlsoRow,
  SystemRow,
  TxCollection,
  TxDb,
  TxQuery,
  UserRow,
  VersionRow,
  VoteRow,
  WatcherRow,
} from "./server/rows";

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
const OPEN_STATUSES: readonly IssueStatus[] = [
  "UNCONFIRMED",
  "CONFIRMED",
  "IN_PROGRESS",
];
const NULLABLE_ISSUE_TEXT_FIELDS = new Set([
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

/**
 * A product key: the human half of PARSER-12.
 *
 * Uppercase letters and digits, starting with a letter, 2 to 10 characters.
 * Short because it is typed and spoken; no hyphen because the hyphen is the
 * separator, and a key containing one would make `PARSER-X-12` ambiguous to
 * parse.
 */
const PRODUCT_KEY_PATTERN = /^[A-Z][A-Z0-9]{1,9}$/;

function requireProductKey(value: string): string {
  const clean = requireNonEmpty(value, "key").toUpperCase();
  if (!PRODUCT_KEY_PATTERN.test(clean)) {
    invalid(
      "key must be 2 to 10 characters, uppercase letters and digits, starting with a letter " +
        "(for example PARSER)",
    );
  }
  return clean;
}

/**
 * The identifier a person uses: `PARSER-12`.
 *
 * Issues are addressed by this everywhere a human reads or types one. The UUID
 * remains the primary key and the thing every foreign key points at -- this is
 * a display and lookup form, not a second identity.
 */
/**
 * Look an issue up by UUID or by its PARSER-12 key.
 *
 * The key path is two reads rather than one, and is not indexed as a pair --
 * the (productId, number) index does the work once the product is known.
 */
async function issueByIdOrKey(input: string): Promise<IssueRow> {
  const parsed = parseIssueKey(input);
  if (!parsed) return getRequired(db.issues, input, "Issue");
  const product = must(await db.products.get({ key: parsed.key }));
  if (!product) notFound("Issue");
  const issue = must(await db.issues.get({ productId: product.id, number: parsed.number }));
  if (!issue) notFound("Issue");
  return issue;
}

function issueKey(product: { key: string }, issue: { number: number }): string {
  return `${product.key}-${issue.number}`;
}

/**
 * A default key from a product name: "Parser" -> PARSER, "Web UI" -> WEBUI.
 *
 * Only a starting point -- the caller can pass one explicitly. Names that
 * reduce to nothing usable (punctuation only, or a leading digit) fall through
 * to `requireProductKey` and are refused there with a message that says what a
 * key looks like, rather than being silently mangled into something the
 * creator did not choose.
 */
function deriveProductKey(name: string): string {
  const letters = name.toUpperCase().replace(/[^A-Z0-9]/g, "");
  return letters.slice(0, 10);
}

/**
 * Split `PARSER-12` back into its parts. Returns null for anything that is not
 * in that shape, so callers can fall through to treating the input as a UUID.
 */
function parseIssueKey(input: string): { key: string; number: number } | null {
  const match = /^([A-Za-z][A-Za-z0-9]{1,9})-(\d{1,9})$/.exec(input.trim());
  if (!match) return null;
  const number = Number(match[2]);
  if (!Number.isSafeInteger(number) || number < 1) return null;
  return { key: match[1].toUpperCase(), number };
}

function conflict(message: string): never {
  throw httpError(409, "CONFLICT", message);
}

function forbidden(message = "You do not have access to this product"): never {
  throw httpError(403, "FORBIDDEN", message);
}

/**
 * Refusing an issue the caller may not see.
 *
 * Separate from `forbidden()` because the reasons are different and the default
 * message is not interchangeable. Every issue-level denial used to report "You do
 * not have access to this product", which is false in the case that matters:
 * the product IS accessible -- a user can list the product's other issues and
 * file new ones -- and only this issue is held in a group they are not in. A
 * reader told the product is off-limits looks for the wrong fix.
 *
 * 403 and not 404. This is deliberate and it does leak the issue's existence:
 * Bugzilla answers "You are not authorized to access bug #N", so the id space
 * is enumerable there too, and a tracker that pretends a restricted issue was
 * never filed cannot explain why its id is skipped in every list. Hiding
 * existence would be the stronger property and it is NOT what this app does.
 * See "Divergences from Bugzilla" in SPEC.md.
 */
function forbiddenIssue(): never {
  forbidden("You do not have access to this issue. It is restricted to a group you are not in.");
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

// Issue-level security groups: Bugzilla's bug_group_map, the mechanism behind a
// confidential security issue inside an otherwise public product. Product-level
// visibility alone cannot express "this ONE issue is restricted".
//
// This existed as a table and nothing read it. `assertIssueVisible` checked only
// the product, so every issueGroups row was decorative and a "restricted" issue was
// readable by anyone who could see its product.
async function canViewIssue(issue: IssueRow, user: UserRow | null): Promise<boolean> {
  if (!(await canViewProduct(issue.productId, user))) return false;
  const restrictions = await readAll(db.issueGroups, { issueId: issue.id });
  if (restrictions.length === 0) return true;
  if (user?.isAdmin) return true;
  if (!user) return false;
  const memberships = await readAll(db.groupMembers, { userId: user.id });
  const groups = new Set(memberships.map((row) => row.groupId));
  return restrictions.some((row) => groups.has(row.groupId));
}

async function assertIssueVisible(issue: IssueRow, identity: PlatformUser | null): Promise<void> {
  if (!(await canViewIssue(issue, await appUserForIdentity(identity)))) forbiddenIssue();
}

/**
 * The write-path counterpart of `assertIssueVisible`, for handlers that already
 * hold the actor.
 *
 * Every mutation that touches an issue used to call `assertCanViewProduct` -- the
 * PRODUCT check only -- so issue-level restriction guarded reads and nothing
 * else. Measured 2026-08-12: a second user got 403 from `issues.get` on a
 * restricted issue and 200 from `comments.add` on the same issue, in the same
 * session. Commenting, resolving, reassigning, CC'ing, marking attachments
 * obsolete and deleting them were all reachable on an issue the caller could not
 * open.
 *
 * If you can't read it, you can't write it.
 */
async function assertIssueAccessible(issue: IssueRow, actor: UserRow | null): Promise<void> {
  if (!(await canViewIssue(issue, actor))) forbiddenIssue();
}

/**
 * Issues the user must not see because of an issue-level restriction.
 *
 * Returned as an exclusion list for the QUERY rather than applied by filtering
 * the result rows: post-filtering a page silently shrinks it, so a viewer with
 * a restricted issue in range gets a short page and the offsets stop meaning what
 * the caller thinks. `$nin` keeps limit/offset honest.
 */
async function hiddenIssueIds(user: UserRow | null): Promise<string[]> {
  const restrictions = await readAll(db.issueGroups);
  if (restrictions.length === 0) return [];
  if (user?.isAdmin) return [];

  const groups = user
    ? new Set((await readAll(db.groupMembers, { userId: user.id })).map((row) => row.groupId))
    : new Set<string>();

  const byIssue = new Map<string, string[]>();
  for (const row of restrictions) {
    byIssue.set(row.issueId, [...(byIssue.get(row.issueId) ?? []), row.groupId]);
  }
  return [...byIssue.entries()]
    .filter(([, required]) => !required.some((groupId) => groups.has(groupId)))
    .map(([issueId]) => issueId);
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

  // `issues.versionId` is NULLABLE, so an omitted version is a legal filing and
  // not a 400 any more. This reversed the rule that a Bugzilla bug is always
  // filed against a version, which held while every row WAS a bug: "version
  // found in" is a defect concept, and requiring it meant a feature request had
  // to name a version it has nothing to do with. A version that IS supplied is
  // still checked to belong to the product.
  if (versionId) {
    const version = await getRequired(db.versions, versionId, "Version");
    if (version.productId !== product.id) invalid("versionId does not belong to productId");
  }
  if (milestoneId) {
    const milestone = await getRequired(db.milestones, milestoneId, "Milestone");
    if (milestone.productId !== product.id) invalid("milestoneId does not belong to productId");
  }
  return { product, component };
}

function issueState(row: IssueRow): IssueState {
  if (!isIssueStatus(row.status)) invalid(`issue ${row.id} has an invalid stored status`);
  // The current DB update builder binds a nullable TEXT `$set: null` as an
  // empty text parameter. Keep the app's logical contract null-shaped at the
  // boundary (and in history/state checks) until that platform seam is fixed.
  const resolution = row.resolution ? row.resolution : null;
  if (resolution !== null && !isIssueResolution(resolution)) {
    invalid(`issue ${row.id} has an invalid stored resolution`);
  }
  return { status: row.status, resolution };
}

/**
 * Blank an issue link that points somewhere the caller cannot look.
 *
 * Excluding the restricted ROW from a list is only half the job: a surviving
 * row that still names the restricted issue in `duplicateOfId` confirms it
 * exists, which is what a confidential issue is hiding.
 */
/**
 * Should this history row be withheld from the viewer?
 *
 * Activity rows are permanent, and several carry ANOTHER issue's id as their
 * value: `dependsOn` and `duplicateOfId` both do. The write that created the
 * row required access to both issues at the time, but the row survives a later
 * restriction, so replaying history re-leaks what the graph and duplicate
 * endpoints withhold.
 *
 * `bug_group` rows go further and are dropped whenever anything is hidden from
 * this viewer: which groups an issue is restricted to is itself the shape of the
 * security model.
 */
const ISSUE_REFERENCE_FIELDS = new Set(["dependsOn", "duplicateOfId", "blocks"]);

function hidesRestrictedReference(
  activity: ActivityRow,
  hidden: ReadonlySet<string>,
): boolean {
  if (activity.fieldName === "issue_group") return hidden.size > 0;
  if (!ISSUE_REFERENCE_FIELDS.has(activity.fieldName)) return false;
  return [activity.oldValue, activity.newValue].some(
    (value) => typeof value === "string" && hidden.has(value),
  );
}

function maskHiddenIssueLinks(row: IssueRow, hidden: ReadonlySet<string>): IssueRow {
  if (!row.duplicateOfId || !hidden.has(row.duplicateOfId)) return row;
  return { ...row, duplicateOfId: null };
}

function normalizeIssueRow(row: IssueRow): IssueRow {
  const out = { ...row } as IssueRow & Record<string, unknown>;
  for (const field of NULLABLE_ISSUE_TEXT_FIELDS) {
    if (out[field] === "" || out[field] === undefined) out[field] = null;
  }
  if (out.deadline === undefined) out.deadline = null;
  return out;
}

function logicalIssueValue(field: string, value: unknown): unknown {
  return NULLABLE_ISSUE_TEXT_FIELDS.has(field) && (value === "" || value === undefined)
    ? null
    : value;
}

function asTrackedRecord(value: object): Record<string, string | number | boolean | null | undefined> {
  const record = {
    ...(value as Record<string, string | number | boolean | null | undefined>),
  };
  for (const field of NULLABLE_ISSUE_TEXT_FIELDS) {
    if (record[field] === "" || record[field] === undefined) record[field] = null;
  }
  return record;
}

async function insertActivities(
  activities: TxCollection<ActivityRow>,
  issueId: string,
  actorId: string,
  changes: readonly TrackedFieldChange[],
): Promise<void> {
  if (changes.length === 0) return;
  await activities.insertMany(
    changes.map((change) => ({
      issueId,
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
  issueId: string,
  actorId: string,
  before: object,
  after: object,
  fieldNames: readonly string[],
): Promise<void> {
  await insertActivities(
    activities,
    issueId,
    actorId,
    diffTrackedFields(asTrackedRecord(before), asTrackedRecord(after), fieldNames),
  );
}

async function recordRelatedChange(
  activities: TxCollection<ActivityRow>,
  issueId: string,
  actorId: string,
  fieldName: string,
  oldValue: string | null,
  newValue: string | null,
): Promise<void> {
  if (oldValue === newValue) return;
  await insertActivities(activities, issueId, actorId, [
    { fieldName, oldValue, newValue },
  ]);
}

async function updateIssueWithHistory(
  id: string,
  actorId: string,
  makePatch: (before: IssueRow) => DbPatch | Promise<DbPatch>,
  trackedFields: readonly string[],
): Promise<IssueRow> {
  const result = await db.transaction(async (tx) => {
    const before = await getTxRequired(tx.issues, id, "Issue");
    const candidate = await makePatch(before);
    const patch = Object.fromEntries(
      Object.entries(candidate).filter(
        ([field, value]) =>
          logicalIssueValue(field, before[field as keyof IssueRow]) !==
          logicalIssueValue(field, value),
      ),
    );
    if (Object.keys(patch).length === 0) return { row: normalizeIssueRow(before), changed: [] };
    const after = await tx.issues.update(id, patch);
    if (!after) notFound("Issue");
    await recordChanges(tx.activities, id, actorId, before, after, trackedFields);
    return {
      row: normalizeIssueRow(after),
      changed: Object.keys(patch).filter((field) => trackedFields.includes(field)),
    };
  }, { isolationLevel: "serializable" });

  const { row, changed } = must(result);

  // Fanout lives HERE rather than at each call site, because putting it at
  // call sites is exactly how it ended up on two of the ten mutations that
  // change an issue. It covers the EIGHT that route through this helper;
  // issues.markDuplicate runs its own transaction for cycle detection and
  // notifies itself, and issues.create deliberately does not notify at all. issues.reassign was silent, so a new assignee was never told
  // they had been given an issue -- the single most useful notification a tracker
  // sends.
  //
  // Nothing is sent when the patch was empty: `changed` is derived from the
  // fields that actually differed, so a no-op update does not wake anyone.
  if (changed.length > 0) {
    await notifyIssueChange(row, actorId, `${row.summary} was updated (${changed.join(", ")})`);
  }
  return row;
}

function rejectUnsupportedIssueNullClears(
  before: IssueRow,
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
  return rows.map((row) => ({ from: row.issueId, to: row.dependsOnId }));
}

function duplicateEdges(rows: readonly IssueRow[]): DirectedEdge[] {
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

function attachmentStorageKey(issueId: string, filename: string): string {
  return `${issueId}/${crypto.randomUUID()}-${safeFilename(filename)}`;
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

async function issueIdForFlag(flag: FlagRow): Promise<string> {
  if (flag.issueId) return flag.issueId;
  if (!flag.attachmentId) invalid("stored flag has no target");
  return (await getRequired(db.attachments, flag.attachmentId, "Attachment")).issueId;
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
// Issues
// ---------------------------------------------------------------------------

type CreateIssueInput = {
  productId: string;
  componentId: string;
  summary: string;
  description: string;
  // Optional, and NOT defaulted to a version by the server: an enhancement has
  // no "version found in", so the column is nullable and an omitted version
  // stays omitted.
  versionId?: string | null;
  milestoneId?: string | null;
  kind?: IssueKind;
  severity?: IssueSeverity;
  priority?: IssuePriority;
  assigneeId?: string | null;
  qaContactId?: string | null;
  whiteboard?: string | null;
  opSys?: string;
  platform?: string;
  url?: string | null;
  confirmed?: boolean;
  deadline?: number | null;
};

export const createIssue = mutation(
  async ({
    productId,
    componentId,
    summary,
    description,
    versionId,
    milestoneId,
    kind = "defect",
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
  }: CreateIssueInput) => {
    const actor = await requireActor();
    if (!ISSUE_KINDS.includes(kind)) invalid("invalid kind");
    if (!ISSUE_SEVERITIES.includes(severity)) invalid("invalid severity");
    if (!ISSUE_PRIORITIES.includes(priority)) invalid("invalid priority");
    const structure = await validateProductChildren(
      requireId(productId, "productId"),
      requireId(componentId, "componentId"),
      versionId,
      milestoneId,
    );
    await assertCanViewProduct(structure.product.id, actor);
    if (!structure.product.isActive || !structure.component.isActive) {
      conflict("New issues require an active product and component");
    }

    const isConfirmed = confirmed === true || !structure.product.allowsUnconfirmed;
    const status: IssueStatus = isConfirmed ? "CONFIRMED" : "UNCONFIRMED";
    const cleanSummary = requireNonEmpty(summary, "summary").slice(0, 500);
    const cleanDescription = requireNonEmpty(description, "description");
    const result = await db.transaction(async (tx) => {
      // Allocated inside the transaction, from the rows themselves rather than
      // a counter that could drift from them. Two concurrent files can read
      // the same max; the (productId, number) unique index rejects the loser
      // and the retry below re-reads. Without that index this is a lost
      // update that silently gives two issues the same key.
      const siblings = await readAllTx(tx.issues, { productId: structure.product.id });
      const nextNumber =
        siblings.reduce((highest, row) => Math.max(highest, row.number ?? 0), 0) + 1;
      const issue = await tx.issues.insert({
        number: nextNumber,
        productId: structure.product.id,
        componentId: structure.component.id,
        ...(versionId ? { versionId } : {}),
        ...(milestoneId ? { milestoneId } : {}),
        summary: cleanSummary,
        kind,
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
        issueId: issue.id,
        authorId: actor.id,
        body: cleanDescription,
        commentNumber: 0,
        isPrivate: false,
      });
      await recordChanges(
        tx.activities,
        issue.id,
        actor.id,
        {},
        issue,
        ISSUE_CREATION_FIELDS,
      );
      return issue;
    });
    const created = must(result);
    // Filing is an event too. Watchers only ever heard about CHANGES, so
    // watching someone whose new issues never reached you was close to
    // pointless -- and in Bugzilla a watch delivers their bugmail, which
    // starts at the report. The fanout already excludes the actor, so the
    // reporter does not notify themselves.
    await notifyIssueChange(created, actor.id, `${created.summary} was filed`);
    return created;
  },
  { id: "issues.create" },
);

export const getIssue = query(
  async ({ id }: { id: string }) => {
    // Accepts either form. A person arrives with PARSER-12 -- from a commit
    // message, a chat, the address bar -- while every internal link still
    // carries the UUID. Refusing the human form here would mean the
    // identifier the app puts on screen is not one it accepts back.
    const storedIssue = await issueByIdOrKey(id);
    // Resolved once and reused: issues.get is anonymous, so `optionalIdentity()`
    // is frequently null and `appUserForIdentity(null)` is the anonymous case
    // rather than an error.
    const identity = optionalIdentity();
    await assertIssueVisible(storedIssue, identity);
    const hidden = new Set(await hiddenIssueIds(await appUserForIdentity(identity)));
    const issue = maskHiddenIssueLinks(normalizeIssueRow(storedIssue), hidden);
    const [activityRows, product, component] = await Promise.all([
      readAll(db.activities, { issueId: issue.id }),
      db.products.get(issue.productId),
      db.components.get(issue.componentId),
    ]);
    // Everyone this issue names, resolved once. The page showed raw ids for the
    // assignee, reporter and QA contact while the CC panel beside them showed
    // real names, because `cc.list` joins its user and nothing else did.
    // `publicUserView`, since `issues.get` is anonymous.
    const people = Object.fromEntries(
      (
        await readByIds(
          db.users,
          // assignee and QA contact are both nullable; an unset one must not
          // become a null inside the `$in` list.
          // Activity actors too. The history log records WHO changed each
          // field and the tab could not say so: without these ids the panel
          // either printed a raw `usr_...` or, as it did, left the person out
          // of the record entirely. A history that cannot name who acted is
          // the half of an audit trail that does not audit.
          [
            issue.assigneeId,
            issue.reporterId,
            issue.qaContactId,
            ...activityRows.map((activity) => activity.actorId),
          ].filter(
            (value): value is string => typeof value === "string" && value.length > 0,
          ),
        )
      ).map((user) => [user.id, publicUserView(user)]),
    );

    // Issues named by the LOG rather than by a column.
    //
    // `dependsOn`, `blocks` and `duplicateOfId` store another issue's typed id
    // as the activity value, and the history rendered it verbatim: "set
    // Depends On to issue_0346W0Ole6amXDN9RKvzW6". That is the raw-id leak this
    // app has a whole spec class about, and it survived because the spec's
    // fixture -- "an issue with everything hung off it" -- had no dependency, so
    // the row it would have caught was never written.
    //
    // Resolved here rather than in the client for the same reason `people` is:
    // the viewer's access was already decided above, so a reference they may
    // not see is simply absent from the map.
    const referencedIssueIds = [
      ...new Set(
        [
          ...activityRows
            .filter((activity) => ISSUE_REFERENCE_FIELDS.has(activity.fieldName))
            .flatMap((activity) => [activity.oldValue, activity.newValue]),
          issue.duplicateOfId,
        ]
          .filter(
            (value): value is string =>
              typeof value === "string" && value.length > 0 && !hidden.has(value),
          ),
      ),
    ];
    const referencedIssues = referencedIssueIds.length > 0 ? await readByIds(db.issues, referencedIssueIds) : [];
    const referencedProducts =
      referencedIssues.length > 0
        ? await readByIds(db.products, [...new Set(referencedIssues.map((b) => b.productId))])
        : [];
    const referencedKeys = new Map(referencedProducts.map((prod) => [prod.id, prod.key]));
    const issueRefs = Object.fromEntries(
      referencedIssues.map((b) => {
        const key = referencedKeys.get(b.productId);
        return [b.id, { label: key ? `${key}-${b.number}` : b.id, summary: b.summary }];
      }),
    );

    return {
      // The same denormalised key searchIssues carries, so an issue from EITHER
      // endpoint can name itself without a second lookup. The dashboard builds
      // its rows from issues.get and would otherwise be the one surface that
      // could not.
      // OPTIONAL, so a plain IssueRow -- what every mutation returns -- is still
      // assignable. The label falls back when it is absent, so the only cost of
      // a mutation not carrying it is one render without the key, and the
      // alternative is threading it through twenty return statements.
      issue: { ...issue, productKey: must(product)?.key ?? null } as IssueRow & {
        productKey?: string | null;
      },
      people,
      issueRefs,
      product: must(product),
      component: must(component),
      // The history is filtered too. Several activity rows carry ANOTHER
      // issue's id in their value -- `dependsOn` and `duplicateOfId` both do --
      // so an unfiltered stream re-leaks exactly what the graph and duplicate
      // endpoints were hardened to withhold. The write required access at the
      // time; the row outlives that. `bug_group` rows are dropped for
      // non-privileged viewers outright: which groups an issue is restricted to
      // is itself security information.
      activities: activityRows
        .sort((left, right) => left.created_at - right.created_at)
        .filter((activity) => !hidesRestrictedReference(activity, hidden))
        .map((activity) => ({
          // WHO, which the log has always recorded and this projection threw
          // away, so the history tab could not name the person who made a
          // change. It is the same publicUserView identity the rest of the
          // payload carries -- no new exposure, just stopping the discard.
          actorId: activity.actorId,
          fieldName: activity.fieldName,
          oldValue: activity.oldValue ?? null,
          newValue: activity.newValue ?? null,
          changedAt: activity.created_at,
        })),
    };
  },
  { id: "issues.get" },
);

type IssueSearchInput = {
  productId?: string;
  componentId?: string;
  versionId?: string;
  milestoneId?: string;
  status?: IssueStatus | IssueStatus[];
  resolution?: IssueResolution | IssueResolution[] | null;
  kind?: IssueKind | IssueKind[];
  severity?: IssueSeverity | IssueSeverity[];
  priority?: IssuePriority | IssuePriority[];
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

async function searchIssuesInternal(
  input: IssueSearchInput,
  identity: PlatformUser | null,
  extraFilter?: DbFilter,
  // The annotation carries productKey: without it the declared type erases the
  // field the client needs to render PARSER-12, and the flash comes back with
  // nothing failing to typecheck.
): Promise<(IssueRow & { productKey?: string | null })[]> {
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
    ...(input.kind ? { kind: inFilter(input.kind) } : {}),
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

  // Issue-level restrictions are applied to every search, not only to issues.get.
  // Enforcing on the detail route alone would keep a restricted issue's summary,
  // status and assignee listed on the issue list and in reports -- which is most
  // of what a confidential issue is trying not to leak.
  // Chunked for the same reason every `$in` in this file is: the list is
  // unbounded (one entry per restricted issue the viewer cannot see) and each
  // entry becomes a bind parameter, so one `NOT IN` would grow without limit.
  // `$nin` chunks cleanly where `$in` would not -- excluding A and excluding B
  // is the AND of the two, whereas including A or B is the OR.
  const hidden = await hiddenIssueIds(await appUserForIdentity(identity));
  for (const page of chunks(hidden)) clauses.push({ id: { $nin: page } });

  const filter = clauses.length === 1 ? clauses[0] : { $and: clauses };
  const sortBy = input.sortBy ?? "updated_at";
  const direction = input.sortDirection ?? -1;
  // The same `hidden` set that built the $nin above also masks outward links.
  // Excluding restricted ROWS is not the whole job: a public issue that is a
  // duplicate of a restricted one still NAMES it in duplicateOfId, and
  // issues.search is anonymous.
  const hiddenSet = new Set(hidden);
  const rows = must(
    await db.issues
      .find(filter)
      .sort({ [sortBy]: direction })
      .skip(clampOffset(input.offset))
      .limit(clampLimit(input.limit)),
  ).map((row) => maskHiddenIssueLinks(normalizeIssueRow(row), hiddenSet));

  // The product KEY travels with the row.
  //
  // An issue is displayed as PARSER-12, and the key half used to be fetched
  // separately by the client, so every time the rows changed the id column
  // rendered a full UUID until that second request landed and then snapped to
  // the short form -- a visible flash on every sort, filter and page turn.
  //
  // The server already knows the key, and one lookup here covers the whole
  // page: at most one product per row, in practice a handful. Denormalised
  // onto the row rather than returned alongside it because every consumer of
  // an issue wants to be able to name it.
  const keysByProduct = new Map(
    (await readByIds(db.products, rows.map((row) => row.productId))).map((product) => [
      product.id,
      product.key,
    ]),
  );
  return rows.map((row) => ({ ...row, productKey: keysByProduct.get(row.productId) ?? null }));
}

export const searchIssues = query(
  async ({
    productId,
    componentId,
    versionId,
    milestoneId,
    status,
    resolution,
    kind,
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
  }: IssueSearchInput) =>
    searchIssuesInternal(
      {
        productId,
        componentId,
        versionId,
        milestoneId,
        status,
        resolution,
        kind,
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
  { id: "issues.search" },
);

type GeneralIssuePatch = {
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

const GENERAL_ISSUE_FIELDS = [
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

export const updateIssue = mutation(
  async ({ id, changes }: { id: string; changes: GeneralIssuePatch }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    const keys = Object.keys(changes);
    if (keys.length === 0) invalid("changes must contain at least one field");
    if (keys.some((key) => !(GENERAL_ISSUE_FIELDS as readonly string[]).includes(key))) {
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
    return updateIssueWithHistory(
      id,
      actor.id,
      (before) => {
        rejectUnsupportedIssueNullClears(before, changes);
        return { ...changes };
      },
      GENERAL_ISSUE_FIELDS,
    );
  },
  { id: "issues.update" },
);

export const changeIssueStatus = mutation(
  async ({
    id,
    status,
    resolution,
  }: {
    id: string;
    status: IssueStatus;
    resolution?: IssueResolution | null;
  }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    return updateIssueWithHistory(
      id,
      actor.id,
      (before) => {
        const next = transitionIssueState(issueState(before), { status, resolution });
        // `resolvedAt` is maintained HERE too, not only in issues.resolve /
        // markDuplicate / reopen. This path performs the same transitions --
        // it can move an issue into RESOLVED and back out to CONFIRMED -- and it
        // was leaving the stamp untouched in both directions:
        //   - resolving through here left resolvedAt null, so
        //     reports.timeToResolve never counted the issue while
        //     reports.trend (which mines status history) did, and the two
        //     reports disagreed about the same issue;
        //   - reopening through here left a stale stamp, so timeToResolve
        //     counted a currently-OPEN issue as resolved, with a duration
        //     measured to a resolution that had been undone.
        const wasResolved = !isOpenIssueStatus(issueState(before).status);
        const isResolved = !isOpenIssueStatus(next.status);
        return {
          status: next.status,
          resolution: next.resolution,
          isConfirmed: next.status !== "UNCONFIRMED",
          ...(next.resolution !== "DUPLICATE" ? { duplicateOfId: null } : {}),
          ...(isResolved && !wasResolved
            ? { resolvedAt: portableTimestamp(Date.now()) }
            : {}),
          ...(!isResolved && wasResolved ? { resolvedAt: null } : {}),
        };
      },
      ["status", "resolution", "isConfirmed", "duplicateOfId"],
    );
  },
  { id: "issues.changeStatus" },
);

export const resolveIssue = mutation(
  async ({ id, resolution }: { id: string; resolution: Exclude<IssueResolution, "DUPLICATE"> }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    const updated = await updateIssueWithHistory(
      id,
      actor.id,
      (before) => {
        const next = resolveIssueState(issueState(before), resolution);
        return {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: null,
          isConfirmed: true,
          // Stamped here, and deliberately NOT in trackedFields below: it is
          // derived metadata, not a field a user edited, so it does not belong
          // in the issue's visible history.
          resolvedAt: portableTimestamp(Date.now()),
        };
      },
      ["status", "resolution", "duplicateOfId", "isConfirmed"],
    );
    // No explicit fanout here any more: updateIssueWithHistory sends one for
    // every field it actually changed, and calling it again would notify
    // twice for a single resolve.
    return updated;
  },
  { id: "issues.resolve" },
);

export const reopenIssue = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    return updateIssueWithHistory(
      id,
      actor.id,
      (before) => {
        const next = reopenIssueState(issueState(before));
        return {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: null,
          isConfirmed: true,
          // Cleared on reopen for the same reason it is set on resolve: a
          // reopened issue is not resolved, and leaving a stale stamp would make
          // reports.timeToResolve count it as still-closed.
          resolvedAt: null,
        };
      },
      ["status", "resolution", "duplicateOfId", "isConfirmed"],
    );
  },
  { id: "issues.reopen" },
);

export const markIssueDuplicate = mutation(
  async ({ id, duplicateOfId }: { id: string; duplicateOfId: string }) => {
    const actor = await requireActor();
    const sourceId = requireId(id, "id");
    const [source, target] = await Promise.all([
      getRequired(db.issues, sourceId, "Issue"),
      issueByIdOrKey(duplicateOfId),
    ]);
    if (source.id === target.id) invalid("an issue cannot duplicate itself");
    await assertIssueAccessible(source, actor);
    await assertIssueAccessible(target, actor);

    const result = await db.transaction(
      async (tx) => {
        const before = await getTxRequired(tx.issues, source.id, "Issue");
        await getTxRequired(tx.issues, target.id, "Duplicate target");
        const allIssues = await readAllTx(tx.issues, { duplicateOfId: { $ne: null } });
        if (wouldCreateDirectedCycle(duplicateEdges(allIssues), source.id, target.id)) {
          conflict("duplicate relationship would create a cycle");
        }
        const next = markDuplicateIssueState(issueState(before));
        const after = await tx.issues.update(source.id, {
          status: next.status,
          resolution: next.resolution,
          duplicateOfId: target.id,
          isConfirmed: true,
          // A duplicate is a resolution too, so it is stamped like the others.
          // Missing it here would silently exclude every duplicate from
          // reports.timeToResolve.
          resolvedAt: portableTimestamp(Date.now()),
        });
        if (!after) notFound("Issue");
        await recordChanges(
          tx.activities,
          source.id,
          actor.id,
          before,
          after,
          ["status", "resolution", "duplicateOfId", "isConfirmed"],
        );
        return normalizeIssueRow(after);
      },
      { isolationLevel: "serializable" },
    );
    const marked = must(result);
    // Notified explicitly, because this handler runs its OWN transaction for
    // cycle detection and never goes through updateIssueWithHistory -- where the
    // central fanout lives. Marking a duplicate is a real closure, exactly
    // like issues.resolve, and it was the one closure that told nobody. The
    // comment on the central fanout claimed it covered every issue mutation; it
    // covers the eight that route through that helper.
    await notifyIssueChange(marked, actor.id, `${marked.summary} was closed as a duplicate`);
    return marked;
  },
  { id: "issues.markDuplicate" },
);

export const reassignIssue = mutation(
  async ({ id, assigneeId }: { id: string; assigneeId: string | null }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    if (assigneeId) await getRequired(db.users, assigneeId, "Assignee");
    return updateIssueWithHistory(
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
  { id: "issues.reassign" },
);

/**
 * Reclassify an issue: a defect that turns out to be a feature request, a
 * request that turns out to be a chore.
 *
 * Filing already picks a kind, and without this it could never be corrected --
 * `issues.update` refuses fields with a dedicated mutation, so `kind` would
 * have been a write-once column set by whoever filed. It routes through
 * `updateIssueWithHistory` like setSeverity and setPriority, so the change
 * lands in the issue's history and its watchers hear about it.
 */
export const setIssueKind = mutation(
  async ({ id, kind }: { id: string; kind: IssueKind }) => {
    const actor = await requireActor();
    if (!ISSUE_KINDS.includes(kind)) invalid("invalid kind");
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    return updateIssueWithHistory(id, actor.id, () => ({ kind }), ["kind"]);
  },
  { id: "issues.setKind" },
);

export const setIssueSeverity = mutation(
  async ({ id, severity }: { id: string; severity: IssueSeverity }) => {
    const actor = await requireActor();
    if (!ISSUE_SEVERITIES.includes(severity)) invalid("invalid severity");
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    return updateIssueWithHistory(id, actor.id, () => ({ severity }), ["severity"]);
  },
  { id: "issues.setSeverity" },
);

export const setIssuePriority = mutation(
  async ({ id, priority }: { id: string; priority: IssuePriority }) => {
    const actor = await requireActor();
    if (!ISSUE_PRIORITIES.includes(priority)) invalid("invalid priority");
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);
    return updateIssueWithHistory(id, actor.id, () => ({ priority }), ["priority"]);
  },
  { id: "issues.setPriority" },
);

export const moveIssue = mutation(
  async ({ id, productId, componentId }: { id: string; productId: string; componentId: string }) => {
    const actor = await requireActor();
    const current = await getRequired(db.issues, id, "Issue");
    await assertIssueAccessible(current, actor);

    // The version and milestone are REMAPPED to same-named entries in the
    // target product, which is what Bugzilla does. They cannot simply be
    // carried over -- both are product-scoped, so keeping the old ids would
    // leave the issue pointing at another product's rows -- and they cannot be
    // cleared either, because env.db has no SQL NULL update.
    //
    // BOTH are optional now, and the version arm is new: `issues.versionId` is
    // nullable, so an issue filed without a version -- which is every
    // enhancement -- moves and simply stays without one. This used to refuse
    // that case outright ("this bug has no version and cannot be moved"), a
    // refusal no input could reach while the column was NOT NULL and which
    // would now reject the most ordinary feature request.
    let mappedVersionId: string | undefined;
    if (current.versionId) {
      const currentVersion = await getRequired(db.versions, current.versionId, "Version");
      const targetVersions = await readAll(db.versions, { productId });
      const mappedVersion = targetVersions.find((row) => row.name === currentVersion.name);
      if (!mappedVersion) {
        conflict(
          `the target product has no version named "${currentVersion.name}"; ` +
            `create it there before moving this issue`,
        );
      }
      mappedVersionId = mappedVersion.id;
    }

    let mappedMilestoneId: string | undefined;
    if (current.milestoneId) {
      const currentMilestone = await getRequired(db.milestones, current.milestoneId, "Milestone");
      const targetMilestones = await readAll(db.milestones, { productId });
      const mapped = targetMilestones.find((row) => row.name === currentMilestone.name);
      if (!mapped) {
        conflict(
          `the target product has no milestone named "${currentMilestone.name}"; ` +
            `create it there before moving this issue`,
        );
      }
      mappedMilestoneId = mapped.id;
    }

    const structure = await validateProductChildren(
      productId,
      componentId,
      mappedVersionId,
      mappedMilestoneId,
    );
    await assertCanViewProduct(structure.product.id, actor);

    return updateIssueWithHistory(
      id,
      actor.id,
      () => ({
        productId: structure.product.id,
        componentId: structure.component.id,
        ...(mappedVersionId ? { versionId: mappedVersionId } : {}),
        ...(mappedMilestoneId ? { milestoneId: mappedMilestoneId } : {}),
      }),
      ["productId", "componentId", "versionId", "milestoneId"],
    );
  },
  { id: "issues.move" },
);

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

export const addComment = mutation(
  async ({
    issueId,
    body,
    isPrivate = false,
  }: {
    issueId: string;
    body: string;
    isPrivate?: boolean;
  }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);
    const cleanBody = requireNonEmpty(body, "body");

    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.issues, issueId, "Issue");
      const after = await tx.issues.update(issueId, {
        commentCount: { $inc: 1 },
      });
      if (!after) notFound("Issue");
      const commentNumber = after.commentCount - 1;
      const comment = await tx.comments.insert({
        issueId,
        authorId: actor.id,
        body: cleanBody,
        commentNumber,
        isPrivate,
      });
      await recordChanges(
        tx.activities,
        issueId,
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
    await notifyIssueChange(
      issue,
      actor.id,
      `New comment on ${issue.summary}`,
      isPrivate ? undefined : cleanBody.slice(0, 200),
    );
    return comment;
  },
  { id: "comments.add" },
);

export const listComments = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = optionalIdentity();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueVisible(issue, identity);
    const user = await appUserForIdentity(identity);
    const filter: DbFilter = user?.isAdmin
      ? { issueId }
      : user
        ? {
            $and: [
              { issueId },
              { $or: [{ isPrivate: false }, { authorId: user.id }] },
            ],
          }
        : { issueId, isPrivate: false };
    const rows = must(
      await db.comments.find(filter).sort({ commentNumber: 1 }).limit(500),
    );
    // The author is resolved here rather than left to the client, following
    // `cc.list`. A comment is attributable or it is not worth much, and the
    // client cannot do this itself on a public issue: `users.list` requires
    // authentication, so an anonymous reader would be stuck with the id.
    // `publicUserView` and not the row -- this is reachable anonymously.
    const authors = await readByIds(db.users, rows.map((row) => row.authorId));
    const byId = new Map(authors.map((user) => [user.id, publicUserView(user)]));
    return rows.map((row) => ({ ...row, author: byId.get(row.authorId) ?? null }));
  },
  { id: "comments.list" },
);

export const editComment = mutation(
  async ({ id, body }: { id: string; body: string }) => {
    const actor = await requireActor();
    const comment = await getRequired(db.comments, id, "Comment");
    const issue = await getRequired(db.issues, comment.issueId, "Issue");
    await assertIssueAccessible(issue, actor);
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
        before.issueId,
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
    const issue = await getRequired(db.issues, comment.issueId, "Issue");
    await assertIssueAccessible(issue, actor);
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
        before.issueId,
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
    issueId,
    commentId,
    filename,
    contentBase64,
    contentType = "application/octet-stream",
    description,
    isPatch = false,
  }: {
    issueId: string;
    /** The comment this file arrived with, if it arrived with one. */
    commentId?: string | null;
    filename: string;
    contentBase64: string;
    contentType?: string;
    description?: string | null;
    isPatch?: boolean;
  }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);
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

    // A file may name the comment it arrived with, but only a comment ON THIS
    // ISSUE. Without the check, a caller could hang a file off someone else's
    // comment and it would render under their name, on an issue they may not be
    // able to read -- the id is caller-supplied, so it is checked rather than
    // trusted.
    let attachedComment: string | null = null;
    if (commentId) {
      const comment = await getRequired(db.comments, requireId(commentId, "commentId"), "Comment");
      if (comment.issueId !== issue.id) invalid("that comment belongs to a different issue");
      attachedComment = comment.id;
    }

    const storageKey = attachmentStorageKey(issueId, cleanFilename);
    const put = parseNativeJson<{ bucket: string; key: string; size: number }>(
      await storage().put(ATTACHMENT_BUCKET, storageKey, cleanBase64, cleanType),
    );
    const result = await db.transaction(async (tx) => {
      const attachment = await tx.attachments.insert({
        issueId,
        ...(attachedComment ? { commentId: attachedComment } : {}),
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
        issueId,
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
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueVisible(issue, identity);
    return must(
      await db.attachments.find({ issueId }).sort({ created_at: 1 }).limit(500),
    );
  },
  { id: "attachments.list" },
);

export const getAttachment = query(
  async ({ id }: { id: string }) => {
    const identity = requireIdentity();
    const attachment = await getRequired(db.attachments, id, "Attachment");
    const issue = await getRequired(db.issues, attachment.issueId, "Issue");
    await assertIssueVisible(issue, identity);
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
    const issue = await getRequired(db.issues, attachment.issueId, "Issue");
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const before = await getTxRequired(tx.attachments, id, "Attachment");
      const after = await tx.attachments.update(id, { isObsolete });
      if (!after) notFound("Attachment");
      await recordRelatedChange(
        tx.activities,
        before.issueId,
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
    const issue = await getRequired(db.issues, attachment.issueId, "Issue");
    await assertIssueAccessible(issue, actor);

    const raw = await storage().get(ATTACHMENT_BUCKET, attachment.storageKey);
    await storage().delete(ATTACHMENT_BUCKET, attachment.storageKey);
    const result = await db.transaction(async (tx) => {
      const attachmentFlags = await readAllTx(tx.flags, { attachmentId: id });
      for (const flag of attachmentFlags) {
        await tx.flags.delete(flag.id);
        await recordRelatedChange(
          tx.activities,
          attachment.issueId,
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
        deleted.issueId,
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
  async ({ issueId, dependsOnId }: { issueId: string; dependsOnId: string }) => {
    const actor = await requireActor();
    const sourceId = requireId(issueId, "issueId");
    // By UUID or by PARSER-12, for the reason issues.get already gives: the
    // identifier the app puts on screen must be one it accepts back. This is
    // the field where that matters most -- nothing renders an issue's UUID where
    // a person could copy it, so demanding one here made the control usable
    // only by someone reading the address bar.
    const [issue, dependency] = await Promise.all([
      getRequired(db.issues, sourceId, "Issue"),
      issueByIdOrKey(dependsOnId),
    ]);
    if (issue.id === dependency.id) invalid("an issue cannot depend on itself");
    const targetId = dependency.id;
    await assertIssueAccessible(issue, actor);
    // The DEPENDENCY needs the same issue-level check as the issue being edited.
    // Checking only its product let a caller point an edge at a restricted issue
    // they cannot open -- and an edge is itself a disclosure: it confirms the
    // issue exists and names it in the tree. Missed by the earlier sweep because
    // this variable is `dependency`, not `issue`/`current`/`source`/`target`,
    // which is what a mechanical rename catches and a reading pass does not.
    await assertIssueAccessible(dependency, actor);

    const result = await db.transaction(
      async (tx) => {
        const rows = await readAllTx(tx.issueDependencies);
        if (rows.some((row) => row.issueId === issue.id && row.dependsOnId === dependency.id)) {
          conflict("dependency already exists");
        }
        if (wouldCreateDirectedCycle(dependencyEdges(rows), issue.id, dependency.id)) {
          conflict("dependency would create a cycle");
        }
        const inserted = await tx.issueDependencies.insert({
          issueId: issue.id,
          dependsOnId: dependency.id,
        });
        await recordRelatedChange(
          tx.activities,
          issue.id,
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
  async ({ issueId, dependsOnId }: { issueId: string; dependsOnId: string }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);

    // BOTH ends, matching deps.add. Checking only `issueId` made this an
    // existence oracle -- probe `dependsOnId` and a 404 ("Dependency") is
    // distinguishable from a success, which confirms an edge to an issue the
    // caller cannot open -- and let that caller quietly delete a restricted
    // issue's blocker bookkeeping from the public side.
    const dependency = await getRequired(db.issues, dependsOnId, "Dependency");
    await assertIssueAccessible(dependency, actor);

    const result = await db.transaction(async (tx) => {
      const existing = await tx.issueDependencies.get({ issueId, dependsOnId });
      if (!existing) notFound("Dependency");
      await tx.issueDependencies.deleteMany({ issueId, dependsOnId });
      await recordRelatedChange(
        tx.activities,
        issueId,
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
  issues: IssueRow[];
  dependencies: DependencyRow[];
}> {
  const visible = await visibleProductIds(identity, true);
  const productBatches = chunks([...visible]);
  const [issuePages, dependencies] = await Promise.all([
    Promise.all(productBatches.map((ids) => readAll(db.issues, { productId: { $in: ids } }))),
    readAll(db.issueDependencies),
  ]);
  // Issue-level restrictions apply to the graph too. Filtering on product
  // visibility alone put a restricted issue into deps.tree and deps.graph as a
  // named node -- summary included -- for anyone who could see its product.
  // The edge filter below then keeps only edges whose BOTH ends survive, so a
  // hidden issue also stops leaking through its neighbours.
  const hidden = new Set(await hiddenIssueIds(await appUserForIdentity(identity)));
  const issueRows = issuePages.flat().filter((issue) => !hidden.has(issue.id));
  const ids = new Set(issueRows.map((issue) => issue.id));
  return {
    issues: issueRows,
    dependencies: dependencies.filter(
      (edge) => ids.has(edge.issueId) && ids.has(edge.dependsOnId),
    ),
  };
}

const MAX_DEPENDENCY_TREE_NODES = 2_000;

export const dependencyGraph = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const all = await visibleGraphRows(identity);
    const byId = new Map(all.issues.map((issue) => [issue.id, issue]));
    if (!byId.has(issueId)) notFound("Issue");
    const component = new Set(
      weaklyConnectedComponent(dependencyEdges(all.dependencies), issueId),
    );
    return {
      nodes: [...component]
        .map((id) => byId.get(id))
        .filter((issue): issue is IssueRow => issue !== undefined)
        .map(normalizeIssueRow),
      edges: all.dependencies.filter(
        (edge) => component.has(edge.issueId) && component.has(edge.dependsOnId),
      ),
    };
  },
  { id: "deps.graph" },
);

type DependencyTreeNode = {
  issue: IssueRow;
  dependencies: DependencyTreeNode[];
  cycle?: true;
};

export const dependencyTree = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const all = await visibleGraphRows(identity);
    const byId = new Map(all.issues.map((issue) => [issue.id, issue]));
    if (!byId.has(issueId)) notFound("Issue");
    const children = new Map<string, string[]>();
    for (const edge of all.dependencies) {
      const values = children.get(edge.issueId) ?? [];
      values.push(edge.dependsOnId);
      children.set(edge.issueId, values);
    }
    let expanded = 0;
    const build = (id: string, path: ReadonlySet<string>): DependencyTreeNode => {
      expanded += 1;
      if (expanded > MAX_DEPENDENCY_TREE_NODES) {
        conflict(`dependency tree exceeds ${MAX_DEPENDENCY_TREE_NODES} expanded nodes`);
      }
      const storedIssue = byId.get(id);
      if (!storedIssue) notFound("Dependency issue");
      const issue = normalizeIssueRow(storedIssue);
      if (path.has(id)) return { issue, dependencies: [], cycle: true };
      const nextPath = new Set(path).add(id);
      return {
        issue,
        dependencies: (children.get(id) ?? []).map((child) => build(child, nextPath)),
      };
    };
    return build(issueId, new Set());
  },
  { id: "deps.tree" },
);

export const listDuplicates = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const visible = await visibleProductIds(identity, true);
    const all = (
      await Promise.all(
        chunks([...visible]).map((ids) => readAll(db.issues, { productId: { $in: ids } })),
      )
    ).flat();

    // Issue-level restrictions apply here too. This filtered on product
    // visibility alone, and a duplicate cluster is the classic way a
    // confidential issue becomes reachable: a PUBLIC issue marked as a duplicate
    // of a restricted one put the restricted issue's whole row -- summary,
    // status, assignee -- into this response for anyone who could see the
    // public one.
    //
    // The filter runs BEFORE the existence check, not after. Checking
    // existence against the unfiltered set makes this an oracle: a caller
    // could tell a restricted issue id (404 vs a result) from a nonexistent one.
    const hidden = new Set(await hiddenIssueIds(await appUserForIdentity(identity)));
    const issues = all.filter((issue) => !hidden.has(issue.id));

    if (!issues.some((issue) => issue.id === issueId)) notFound("Issue");
    const ids = new Set(weaklyConnectedComponent(duplicateEdges(issues), issueId));
    // `duplicateOfId` is masked when it points at an issue the caller cannot see.
    // Dropping the restricted ROW is not enough on its own: the surviving
    // public row still named the restricted issue's id, which confirms it exists
    // -- the same disclosure the dependency-edge check closes.
    return issues
      .filter((issue) => ids.has(issue.id))
      .map((issue) => maskHiddenIssueLinks(normalizeIssueRow(issue), hidden));
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
  async ({ issueId, keywordId }: { issueId: string; keywordId: string }) => {
    const actor = await requireActor();
    const [issue, keyword] = await Promise.all([
      getRequired(db.issues, issueId, "Issue"),
      getRequired(db.keywords, keywordId, "Keyword"),
    ]);
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.issueKeywords.get({ issueId, keywordId });
      if (existing) return existing;
      const row = await tx.issueKeywords.insert({ issueId, keywordId });
      await recordRelatedChange(
        tx.activities,
        issueId,
        actor.id,
        "keywords",
        null,
        keyword.name,
      );
      return row;
      // Serializable for the same reason as cc.add: idempotent get-then-insert.
    }, { isolationLevel: "serializable" });
    return must(result);
  },
  { id: "keywords.attach" },
);

export const detachKeyword = mutation(
  async ({ issueId, keywordId }: { issueId: string; keywordId: string }) => {
    const actor = await requireActor();
    const [issue, keyword] = await Promise.all([
      getRequired(db.issues, issueId, "Issue"),
      getRequired(db.keywords, keywordId, "Keyword"),
    ]);
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.issueKeywords.get({ issueId, keywordId });
      if (!existing) notFound("Issue keyword");
      await tx.issueKeywords.deleteMany({ issueId, keywordId });
      await recordRelatedChange(
        tx.activities,
        issueId,
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
    issueId,
    attachmentId,
    requesteeId,
    status,
  }: {
    flagTypeId: string;
    issueId?: string | null;
    attachmentId?: string | null;
    requesteeId?: string | null;
    status: "+" | "-" | "?";
  }) => {
    const actor = await requireActor();
    if (!["+", "-", "?"].includes(status)) invalid("flag status must be +, -, or ?");
    const flagType = await getRequired(db.flagTypes, flagTypeId, "Flag type");
    if (flagType.targetType !== "issue" && flagType.targetType !== "attachment") {
      invalid("flag type has an invalid targetType");
    }
    const target = assertFlagTarget(flagType.targetType, { issueId, attachmentId });
    const targetIssueId = target.issueId
      ? target.issueId
      : (await getRequired(db.attachments, target.attachmentId!, "Attachment")).issueId;
    const issue = await getRequired(db.issues, targetIssueId, "Issue");
    await assertIssueAccessible(issue, actor);
    if (flagType.productId && flagType.productId !== issue.productId) {
      invalid("flag type does not apply to the target product");
    }
    if (status === "?" && !flagType.isRequestable) {
      invalid("this flag type is not requestable");
    }
    if (requesteeId) await getRequired(db.users, requesteeId, "Requestee");

    const targetFilter: DbFilter = target.issueId
      ? { flagTypeId, issueId: target.issueId }
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
            ...(target.issueId ? { issueId: target.issueId } : {}),
            ...(target.attachmentId ? { attachmentId: target.attachmentId } : {}),
            setterId: actor.id,
            ...(requesteeId ? { requesteeId } : {}),
            status,
          });
      if (!flag) notFound("Flag");
      await recordRelatedChange(
        tx.activities,
        targetIssueId,
        actor.id,
        `flag.${flagType.name}`,
        existing?.status ?? null,
        status,
      );
      await recordRelatedChange(
        tx.activities,
        targetIssueId,
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
    const [issueId, flagType] = await Promise.all([
      issueIdForFlag(flag),
      getRequired(db.flagTypes, flag.flagTypeId, "Flag type"),
    ]);
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const deleted = await tx.flags.delete(id);
      if (!deleted) notFound("Flag");
      await recordRelatedChange(
        tx.activities,
        issueId,
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

    // A flag row names its issue, and the issue can be restricted AFTER the flag
    // was set -- the request survives the access that created it. Being named
    // on the flag is not itself permission to know the issue still exists, so
    // rows pointing at a now-hidden issue are dropped here as they are
    // everywhere else. Attachment-scoped flags carry no issueId and are kept.
    const hidden = new Set(await hiddenIssueIds(user));
    const visible = (flag: FlagRow) => !flag.issueId || !hidden.has(flag.issueId);

    return {
      setByMe: must(setByMe).filter(visible).map(hydrate),
      requestedOfMe: must(requestedOfMe).filter(visible).map(hydrate),
    };
  },
  { id: "flags.listRequests" },
);

// The flags currently on an issue, and on each of its attachments.
//
// WHY THIS EXISTS. Without it the only way a client could know an issue's flags
// was to replay the `activities` log and reconstruct them, which is wrong for
// any multiplicable flag type (several live flags of one type collapse to the
// last write) and leaves `flags.clear` unusable: clearing needs a flag id, and
// the id was only ever returned by `flags.set`, so a flag set in an earlier
// session could never be cleared at all. Reconstructing state from a history
// log is not a substitute for reading it.
export const listFlags = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueVisible(issue, identity);

    const attachments = await readAll(db.attachments, { issueId });
    const attachmentIds = new Set(attachments.map((row) => row.id));

    // Attachment flags carry no issueId, so they are found through the issue's
    // attachments rather than in one query.
    const [onIssue, types] = await Promise.all([
      readAll(db.flags, { issueId }),
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
      ...onIssue.map((flag) => flag.setterId),
      ...onIssue.map((flag) => flag.requesteeId),
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
      onIssue: onIssue.map(hydrate),
      onAttachments: onAttachments
        .filter((flag) => flag.attachmentId && attachmentIds.has(flag.attachmentId))
        .map(hydrate),
    };
  },
  { id: "flags.list" },
);

export const addCc = mutation(
  async ({ issueId, userId }: { issueId: string; userId: string }) => {
    const actor = await requireActor();
    const [issue, user] = await Promise.all([
      getRequired(db.issues, issueId, "Issue"),
      getRequired(db.users, userId, "User"),
    ]);
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.issueCc.get({ issueId, userId });
      if (existing) return existing;
      const row = await tx.issueCc.insert({ issueId, userId });
      await recordRelatedChange(
        tx.activities,
        issueId,
        actor.id,
        "cc",
        null,
        user.handle,
      );
      return row;
      // Serializable because this is a get-then-insert that promises
      // idempotence. Under read committed two concurrent identical calls both
      // see "no existing row" and both insert; the unique pair index rejects
      // the loser and `must` rethrows it as a 500 -- so a call whose contract
      // is "adding twice is a no-op" fails instead. The data stays correct
      // either way; what breaks is the promise. SQLite serialises writes, so
      // this is a Postgres-only symptom.
    }, { isolationLevel: "serializable" });
    return must(result);
  },
  { id: "cc.add" },
);

export const removeCc = mutation(
  async ({ issueId, userId }: { issueId: string; userId: string }) => {
    const actor = await requireActor();
    const [issue, user] = await Promise.all([
      getRequired(db.issues, issueId, "Issue"),
      getRequired(db.users, userId, "User"),
    ]);
    await assertIssueAccessible(issue, actor);
    const result = await db.transaction(async (tx) => {
      const existing = await tx.issueCc.get({ issueId, userId });
      if (!existing) notFound("CC entry");
      await tx.issueCc.deleteMany({ issueId, userId });
      await recordRelatedChange(
        tx.activities,
        issueId,
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
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueVisible(issue, identity);
    const rows = (await readAll(db.issueCc, { issueId })).sort(
      (left, right) => left.created_at - right.created_at,
    );
    if (rows.length === 0) return [];
    const users = await readByIds(db.users, rows.map((row) => row.userId));
    const byId = new Map(users.map((user) => [user.id, user]));
    return rows.map((row) => ({ ...row, user: byId.get(row.userId) ?? null }));
  },
  { id: "cc.list" },
);

// The issues the caller is CC'd on -- the reverse of `cc.list`, and the query
// behind the dashboard's "CC'd to me" section.
//
// The schema was already indexed for this direction (`issue_cc_user_idx` on
// issueCc.userId) but no procedure read it, so the dashboard had an index and no
// way to reach it. Visibility is re-applied here rather than trusted from the
// CC row: being CC'd on an issue does not by itself grant access to a product the
// viewer can no longer see.
export const listMyCc = query(
  async ({}: EmptyInput) => {
    const identity = requireIdentity();
    const user = await appUserForIdentity(identity);
    if (!user) return [];

    const rows = await readAll(db.issueCc, { userId: user.id });
    if (rows.length === 0) return [];

    const issues = await readByIds(db.issues, rows.map((row) => row.issueId));
    const visible = await visibleProductIds(identity);
    // BOTH layers, not just the product one. Restricting an issue does not clear
    // its CC list -- in Bugzilla or here -- so a user CC'd before the
    // restriction keeps the row and would otherwise read the issue through this
    // list while `issues.get` returns 403 for the same id. The comment above
    // used to claim visibility was re-applied while only half of it was.
    const hidden = new Set(await hiddenIssueIds(user));
    return issues
      .filter((issue) => visible.has(issue.productId) && !hidden.has(issue.id))
      .map(normalizeIssueRow)
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

/**
 * The number of ids one resolve call will look up.
 *
 * A page of issues is 100 rows and each names one product and one assignee, so
 * 200 covers a full page with room to spare while keeping the request bounded.
 * Callers that need more are paging anyway.
 */
const MAX_RESOLVE_IDS = 200;

function resolveIds(ids: readonly string[] | undefined, noun: string): string[] {
  const unique = [...new Set((ids ?? []).filter((id) => typeof id === "string" && id.length > 0))];
  if (unique.length > MAX_RESOLVE_IDS) {
    invalid(`at most ${MAX_RESOLVE_IDS} ${noun} ids may be resolved at once`);
  }
  return unique;
}

/**
 * Names for a specific set of product ids.
 *
 * The counterpart to `products.list`, and the thing that makes paginating it
 * possible. Every issue table renders a product column by looking the id up in a
 * map, and that map was built by fetching the ENTIRE product list -- so a limit
 * on the list would have silently turned rows past the limit back into
 * `prod_034607nk...`, which is the defect the map exists to prevent.
 *
 * Keyed by the ids actually on screen, so the cost tracks the page rather than
 * the database.
 */
export const resolveProducts = query(
  async ({ ids }: { ids?: string[] }) => {
    const wanted = resolveIds(ids, "product");
    if (wanted.length === 0) return [];
    const identity = optionalIdentity();
    // Filtered by visibility, not just fetched: a product name is a
    // disclosure, and an id the caller guessed must not come back named.
    const visible = await visibleProductIds(identity, false);
    const rows = await readByIds(
      db.products,
      wanted.filter((id) => visible.has(id)),
    );
    // Carries the key as well: an issue table renders PARSER-12 from the
    // product key and the issue number, so resolving the name without the key
    // would leave the id column unable to name its own rows.
    return rows.map((row) => ({ id: row.id, name: row.name, key: row.key }));
  },
  { id: "products.resolve" },
);

/**
 * Handles for a specific set of user ids.
 *
 * `users.list` caps at 100 rows, so the map every issue table built from it was
 * already wrong on a tracker with more than 100 people: issues assigned to the
 * hundred-and-first rendered a raw `user_...` id. This is not a scale worry,
 * it is a live bug at an ordinary size.
 *
 * Returns handles only -- no email, matching `users.list`, which deliberately
 * matches on address without returning it so the endpoint cannot enumerate
 * addresses.
 */
export const resolveUsers = query(
  async ({ ids }: { ids?: string[] }) => {
    const wanted = resolveIds(ids, "user");
    if (wanted.length === 0) return [];
    requireIdentity();
    const rows = await readByIds(db.users, wanted);
    return rows.map((row) => ({ id: row.id, handle: row.handle, name: row.name }));
  },
  { id: "users.resolve" },
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
    key,
    description,
    classification = "Unclassified",
    defaultMilestone,
    allowsUnconfirmed = true,
    isActive = true,
  }: {
    name: string;
    key?: string;
    description?: string | null;
    classification?: string;
    defaultMilestone?: string | null;
    allowsUnconfirmed?: boolean;
    isActive?: boolean;
  }) => {
    await requireActor();
    const cleanName = requireNonEmpty(name, "name");
    if (must(await db.products.get({ name: cleanName }))) conflict("product name already exists");
    // Derived from the name when the caller does not supply one, so filing
    // stays a one-field action, but validated either way: the key is half of
    // every issue identifier and a bad one is not fixable later without
    // renaming every issue reference in prose.
    // Named separately so the failure can say WHY. A name of "!!!" derives to
    // the empty string, and letting that fall through to requireProductKey
    // reported "key is required" for a call that supplied no key on purpose.
    const derived = key ?? deriveProductKey(cleanName);
    if (!derived) {
      invalid(
        `"${cleanName}" has no letters or digits to build a key from -- pass an explicit key, ` +
          "for example PARSER",
      );
    }
    const cleanKey = requireProductKey(derived);
    if (must(await db.products.get({ key: cleanKey }))) {
      // Two messages, because the caller's next move differs. Someone who
      // passed PARSER and was refused knows what to change. Someone who passed
      // only a name is being refused over a key they never saw -- derivation
      // truncates at ten characters, so "Payment Gateway v1" and "Payment
      // Gateway v2" both reduce to PAYMENTGAT, and a bare "key is in use" is
      // baffling.
      //
      // Refused rather than auto-suffixed. A key is half of every issue
      // reference this product will ever have, printed in commit messages and
      // read aloud; silently handing out PAYMENTGA2 because PAYMENTGAT was
      // taken picks something permanent on the creator's behalf.
      if (key) conflict(`product key ${cleanKey} is already in use`);
      conflict(
        `the key derived from "${cleanName}" is ${cleanKey}, which is already in use -- ` +
          "pass an explicit key",
      );
    }
    return must(
      await db.products.insert({
        name: cleanName,
        key: cleanKey,
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
  maxVotesPerIssue?: number;
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
      "maxVotesPerIssue",
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
    // component must have an initial owner -- that is what `issues.create` falls
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
  | "kind"
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
  "kind",
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
  "kind",
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
  if (field === "status" && values.some((item) => !isIssueStatus(item))) {
    invalid("advanced search contains an invalid status");
  }
  if (field === "resolution" && values.some((item) => item !== null && !isIssueResolution(item))) {
    invalid("advanced search contains an invalid resolution");
  }
  if (
    field === "kind" &&
    values.some((item) => !ISSUE_KINDS.includes(item as IssueKind))
  ) {
    invalid("advanced search contains an invalid kind");
  }
  if (
    field === "severity" &&
    values.some((item) => !ISSUE_SEVERITIES.includes(item as IssueSeverity))
  ) {
    invalid("advanced search contains an invalid severity");
  }
  if (
    field === "priority" &&
    values.some((item) => !ISSUE_PRIORITIES.includes(item as IssuePriority))
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
      if (node.value === null && NULLABLE_ISSUE_TEXT_FIELDS.has(node.field)) {
        return { $or: [{ [node.field]: null }, { [node.field]: "" }] };
      }
      return { [node.field]: node.value };
    case "ne":
      if (node.value === null && NULLABLE_ISSUE_TEXT_FIELDS.has(node.field)) {
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
        if (!NULLABLE_ISSUE_TEXT_FIELDS.has(node.field)) {
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
        if (!NULLABLE_ISSUE_TEXT_FIELDS.has(node.field)) {
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
    sortBy?: IssueSearchInput["sortBy"];
    sortDirection?: 1 | -1;
  }) => {
    const identity = requireIdentity();
    return searchIssuesInternal(
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
    case "kind":
      return { kind: clause.value };
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
    const issues = await searchIssuesInternal(
      { limit, offset },
      identity,
      filters.length > 0 ? { $and: filters } : undefined,
    );
    return { clauses, issues };
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
    const rows = must(
      await db.users.find(filter).sort({ name: 1 }).limit(clampLimit(limit, 100)),
    );
    // Same projection as users.get. The `text` filter still MATCHES on email --
    // finding a colleague by address is the point of a people picker -- but the
    // address is not returned, so the endpoint cannot be used to enumerate
    // them. Matching on a value without disclosing it is the distinction.
    return caller?.isAdmin ? rows : rows.map(publicUserView);
  },
  { id: "users.list" },
);

/**
 * The directory view of another person.
 *
 * `users.get` and `users.list` returned the raw row to any authenticated
 * caller: email, isAdmin, isDisabled, and `prefs` -- a free-form bag holding
 * whatever that user stored. Email is PII and the rest is nobody else's
 * business, so a non-admin sees only what an issue page needs to render an
 * assignee. `reports.byAssignee` already projected exactly this shape, so the
 * two directory reads were the outliers.
 *
 * Admins keep the full row: administering accounts needs to see them. The
 * caller's OWN record still comes from `users.me`, which is unprojected.
 */
type PublicUserView = Pick<UserRow, "id" | "handle" | "name" | "timezone">;

function publicUserView(row: UserRow): PublicUserView {
  return { id: row.id, handle: row.handle, name: row.name, timezone: row.timezone };
}

export const getUser = query(
  async ({ id }: { id: string }) => {
    const identity = requireIdentity();
    const actor = await appUserForIdentity(identity);
    const row = await getRequired(db.users, id, "User");
    if (actor?.isAdmin || actor?.id === row.id) return row;
    return publicUserView(row);
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

async function reportIssues(productId?: string): Promise<IssueRow[]> {
  const identity = optionalIdentity();
  const visible = await visibleProductIds(identity);
  if (productId && !visible.has(productId)) return [];
  if (visible.size === 0) return [];

  const rows = productId
    ? await readAll(db.issues, { productId })
    : (
        await Promise.all(
          chunks([...visible]).map((ids) => readAll(db.issues, { productId: { $in: ids } })),
        )
      ).flat();

  // Issue-level restrictions apply to AGGREGATES too. This filtered on product
  // visibility only, so an issue restricted to a security group still landed in
  // reports.summary, byComponent, byAssignee and trend -- leaking its
  // existence, status, severity and assignee to everyone who could see the
  // product. reports.* are anon-accessible, so that audience was "anyone".
  //
  // A count is not a lesser disclosure than a row: "this product has 3 open
  // blockers" is most of what a confidential issue is trying not to say.
  const hidden = new Set(await hiddenIssueIds(await appUserForIdentity(identity)));
  return hidden.size === 0 ? rows : rows.filter((row) => !hidden.has(row.id));
}

async function activityEventsForIssues(
  issueIds: readonly string[],
  filter: DbFilter,
): Promise<ActivityRow[]> {
  return (
    await Promise.all(
      chunks([...new Set(issueIds)]).map((ids) =>
        readAll(db.activities, {
          $and: [{ issueId: { $in: ids } }, filter],
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
    const all = await reportIssues(productId);
    const open = all.filter((issue) => OPEN_STATUSES.includes(issue.status as IssueStatus));
    return {
      total: all.length,
      open: open.length,
      byStatus: countsBy(open, (issue) => issue.status),
      bySeverity: countsBy(open, (issue) => issue.severity),
      byPriority: countsBy(open, (issue) => issue.priority),
    };
  },
  { id: "reports.summary" },
);

export const reportByComponent = query(
  async ({ productId }: ReportInput) => {
    const open = (await reportIssues(productId)).filter((issue) =>
      OPEN_STATUSES.includes(issue.status as IssueStatus),
    );
    if (open.length === 0) return [];
    const components = await readByIds(
      db.components,
      open.map((issue) => issue.componentId),
    );
    const byId = new Map(components.map((component) => [component.id, component]));
    const grouped = new Map<string, IssueRow[]>();
    for (const issue of open) {
      const values = grouped.get(issue.componentId) ?? [];
      values.push(issue);
      grouped.set(issue.componentId, values);
    }
    return [...grouped.entries()]
      .map(([componentId, issues]) => ({
        component: byId.get(componentId) ?? null,
        count: issues.length,
        byStatus: countsBy(issues, (issue) => issue.status),
      }))
      .sort((a, b) => b.count - a.count);
  },
  { id: "reports.byComponent" },
);

export const reportByAssignee = query(
  async ({ productId }: ReportInput) => {
    const open = (await reportIssues(productId)).filter((issue) =>
      OPEN_STATUSES.includes(issue.status as IssueStatus),
    );
    const assigneeIds = [
      ...new Set(open.flatMap((issue) => (issue.assigneeId ? [issue.assigneeId] : []))),
    ];
    const users = await readByIds(db.users, assigneeIds);
    const byId = new Map(users.map((user) => [user.id, user]));
    const grouped = new Map<string, IssueRow[]>();
    for (const issue of open) {
      const key = issue.assigneeId ?? "unassigned";
      const values = grouped.get(key) ?? [];
      values.push(issue);
      grouped.set(key, values);
    }
    return [...grouped.entries()]
      .map(([assigneeId, issues]) => ({
        assignee: assigneeId === "unassigned"
          ? null
          : (() => {
              const user = byId.get(assigneeId);
              return user
                ? { id: user.id, handle: user.handle, name: user.name }
                : null;
            })(),
        count: issues.length,
        byPriority: countsBy(issues, (issue) => issue.priority),
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
    const issues = await reportIssues(productId);
    const activities = await activityEventsForIssues(
      issues.map((issue) => issue.id),
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
    for (const issue of issues) {
      if (issue.created_at < start) continue;
      const bucket = buckets.get(utcDay(issue.created_at));
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
    // newValue="RESOLVED" and took the earliest per issue -- a scan with string
    // matching, and env.db has no raw SQL to make it cheaper. `resolvedAt` is
    // maintained by issues.resolve / issues.markDuplicate / issues.reopen and is
    // indexed (issues_resolved_at_idx).
    //
    // The `typeof === "number"` test is load-bearing and not defensive noise:
    // clearing this column on reopen stores an EMPTY STRING rather than SQL
    // NULL (measured 2026-08-12 on the SQLite dev backend -- a reopened issue
    // reads back `resolvedAt: ""`). A `!= null` test would let that through
    // and `"" - created_at` is NaN, which would silently poison the average.
    const issues = await reportIssues(productId);
    const durations = issues
      .filter((issue) => typeof issue.resolvedAt === "number" && issue.resolvedAt >= start)
      .map((issue) => (issue.resolvedAt as number) - issue.created_at)
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
// `issueGroups` were both readable by the visibility helpers and writable by
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
        isIssueGroup: true,
      }),
    );
  },
  { id: "groups.create" },
);

/**
 * Delete a group, but only while it is not load-bearing.
 *
 * Groups could be created and never removed, so a tracker accumulated them
 * permanently -- this app's own dev database reached 95, every one of them a
 * leftover fixture, on a page whose subject is products. Bugzilla lets an
 * administrator delete a group; this did not, which made the create button a
 * one-way door.
 *
 * It REFUSES while the group still restricts something, and says what. The
 * tempting alternative -- delete the group and let its `issueGroups` and
 * `productGroups` rows dangle -- silently widens who can read those issues, and
 * does it at the moment an admin is tidying up and least expects a visibility
 * change. Memberships are different: a membership grants nothing once the
 * group restricts nothing, so those are purged with the group rather than
 * standing in its way.
 *
 * Purge, not delete: `groups.create` rejects a duplicate name, and a
 * soft-deleted tombstone keeps the unique index occupied, so the name of a
 * deleted group could never be reused.
 */
export const deleteGroup = mutation(
  async ({ id }: { id: string }) => {
    await requireAdmin();
    const groupId = requireId(id, "id");
    const group = must(await db.groups.get(groupId));
    if (!group) notFound("group not found");

    const [issueRestrictions, productRestrictions] = await Promise.all([
      readAll(db.issueGroups, { groupId }),
      readAll(db.productGroups, { groupId }),
    ]);
    if (issueRestrictions.length > 0 || productRestrictions.length > 0) {
      const parts: string[] = [];
      if (issueRestrictions.length > 0) {
        parts.push(`${issueRestrictions.length} issue${issueRestrictions.length === 1 ? "" : "s"}`);
      }
      if (productRestrictions.length > 0) {
        parts.push(
          `${productRestrictions.length} product${productRestrictions.length === 1 ? "" : "s"}`,
        );
      }
      conflict(
        `This group still restricts ${parts.join(" and ")}. Remove those restrictions first, ` +
          `or deleting it would make them readable to everyone.`,
      );
    }

    await db.groupMembers.purgeMany({ groupId });
    await db.groups.purge(groupId);
    return { id: groupId, deleted: true };
  },
  { id: "groups.delete" },
);

/**
 * Flag types, without which the whole flag feature is unreachable.
 *
 * `flags.set`, `flags.clear`, `flags.list` and `flags.listRequests` all take or
 * return a `flagTypeId`, the issue page renders a Flags panel, and SPEC.md lists
 * flags as delivered -- but nothing in the app, the migration or any seed ever
 * inserted a row into `flagTypes`. Every product reported "defines no
 * issue-level flag types" and always would have. Four procedures and a panel
 * were dead surface behind a table nobody could populate.
 *
 * Admin-gated, matching `groups.create`: in Bugzilla flag types are
 * administrator-defined per product, not something a reporter invents while
 * filing.
 */
export const createFlagType = mutation(
  async ({
    name,
    description,
    targetType = "issue",
    isRequestable = true,
    isMultiplicable = false,
    productId,
  }: {
    name: string;
    description?: string | null;
    targetType?: "issue" | "attachment";
    isRequestable?: boolean;
    isMultiplicable?: boolean;
    productId?: string | null;
  }) => {
    await requireAdmin();
    const cleanName = requireNonEmpty(name, "name");
    if (targetType !== "issue" && targetType !== "attachment") {
      invalid('targetType must be "issue" or "attachment"');
    }
    // The unique index is on (name, targetType), so the conflict check has to
    // be as well -- "review" for an issue and "review" for an attachment are two
    // legitimate types, and checking the name alone would refuse the second.
    if (must(await db.flagTypes.get({ name: cleanName, targetType }))) {
      conflict(`a ${targetType} flag type named "${cleanName}" already exists`);
    }
    if (productId) await getRequired(db.products, productId, "Product");
    return must(
      await db.flagTypes.insert({
        name: cleanName,
        ...(description ? { description } : {}),
        targetType,
        isRequestable,
        isMultiplicable,
        // Null means the type applies to every product, which is how
        // `flags.list` already reads it.
        ...(productId ? { productId } : {}),
      }),
    );
  },
  { id: "flagTypes.create" },
);

export const listFlagTypes = query(
  async ({ productId }: { productId?: string | null }) => {
    const identity = optionalIdentity();
    // Scoped to what the caller can see: a flag type carries a product, and
    // naming the types defined on a restricted product would disclose that the
    // product exists. Types with no product are global and always listed.
    const visible = await visibleProductIds(identity, false);
    const rows = await readAll(db.flagTypes, {});
    return rows
      .filter((row) => !row.productId || visible.has(row.productId))
      .filter((row) => !productId || !row.productId || row.productId === productId)
      .sort((left, right) => left.name.localeCompare(right.name));
  },
  { id: "flagTypes.list" },
);

export const listGroups = query(
  async ({}: EmptyInput) => {
    await requireAdmin();
    return readAll(db.groups, {});
  },
  { id: "groups.list" },
);

/**
 * Who is in a group.
 *
 * groups.addMember and groups.removeMember both existed and nothing could
 * list the members between them, so the app could grant access to a group and
 * never show or revoke it. For an access-control feature that is the half
 * that matters: a grant you cannot see is a grant you cannot audit.
 *
 * Admin-only, like the rest of the group surface, and returns the public view
 * of each member rather than the row.
 */
export const listGroupMembers = query(
  async ({ groupId }: { groupId: string }) => {
    await requireAdmin();
    await getRequired(db.groups, groupId, "Group");
    const rows = await readAll(db.groupMembers, { groupId });
    if (rows.length === 0) return [];
    const users = await readByIds(db.users, rows.map((row) => row.userId));
    return users.map(publicUserView).sort((left, right) =>
      (left.name ?? left.handle ?? "").localeCompare(right.name ?? right.handle ?? ""),
    );
  },
  { id: "groups.members" },
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

export const restrictIssue = mutation(
  async ({ issueId, groupId }: { issueId: string; groupId: string }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    if (!(await canViewIssue(issue, actor))) forbiddenIssue();
    await getRequired(db.groups, groupId, "Group");

    // Bugzilla requires you to be IN a group to put an issue into it, and the
    // reason is not bureaucratic: without this an ordinary user could restrict
    // an issue to a group they are not in and lock themselves -- and everyone
    // else outside it -- out of an issue they could previously read.
    if (!actor.isAdmin) {
      const membership = must(await db.groupMembers.get({ groupId, userId: actor.id }));
      if (!membership) forbidden("You must belong to a group to restrict an issue to it");
    }

    // The restriction row and its history entry go in together: a restriction
    // with no audit trail is exactly the change someone later needs to explain.
    return must(
      await db.transaction(async (tx) => {
        const existing = await tx.issueGroups.get({ issueId, groupId });
        if (existing) return existing;
        const row = await tx.issueGroups.insert({ issueId, groupId });
        await recordRelatedChange(tx.activities, issueId, actor.id, "issue_group", null, groupId);
        return row;
        // Serializable for the same reason as cc.add: idempotent
        // get-then-insert. This one guards a restriction, so a spurious 500
        // reads as "the issue may not be protected" and invites a retry that
        // was never needed.
      }, { isolationLevel: "serializable" }),
    );
  },
  { id: "issues.restrict" },
);

export const unrestrictIssue = mutation(
  async ({ issueId, groupId }: { issueId: string; groupId: string }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    if (!(await canViewIssue(issue, actor))) forbiddenIssue();

    // Membership is required to REMOVE a restriction, exactly as it is to add
    // one. Without this, an issue restricted to two groups could have group B's
    // restriction stripped by a member of group A -- who can see the issue
    // through their own group and so passes the check above. That downgrades
    // confidentiality somebody else set, which is why Bugzilla gates edits to
    // an issue's group set in both directions, not just on the way in.
    if (!actor.isAdmin) {
      const membership = must(await db.groupMembers.get({ groupId, userId: actor.id }));
      if (!membership) forbidden("You must belong to a group to remove its restriction");
    }

    return must(
      await db.transaction(async (tx) => {
        const existing = await tx.issueGroups.get({ issueId, groupId });
        if (!existing) notFound("Issue restriction");
        await tx.issueGroups.delete(existing.id);
        await recordRelatedChange(tx.activities, issueId, actor.id, "issue_group", groupId, null);
        return { removed: true };
      }),
    );
  },
  { id: "issues.unrestrict" },
);

// ---------------------------------------------------------------------------
// Voting
//
// The `votes` table and the three product columns (votesPerUser,
// maxVotesPerIssue, votesToConfirm) were added for this and then nothing used
// them: zero server references, so the schema described a feature the app did
// not have and a migration comment claimed it enabled "the classic
// votes-auto-confirm flow" that no code performed.
//
// Bugzilla's rules, which are the point of the three columns:
//   - a user spends at most `votesPerUser` votes across a product,
//   - at most `maxVotesPerIssue` of them on any one issue,
//   - and when an issue reaches `votesToConfirm`, an UNCONFIRMED issue is confirmed.
// A product with the columns left at 0 has voting disabled, which is why 0 is
// the default rather than something permissive.
// ---------------------------------------------------------------------------

export const castVote = mutation(
  async ({ issueId, count }: { issueId: string; count: number }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);

    if (!Number.isInteger(count) || count < 0) invalid("count must be a non-negative integer");
    const product = await getRequired(db.products, issue.productId, "Product");
    if (product.votesPerUser <= 0) conflict("voting is disabled for this product");
    if (product.maxVotesPerIssue > 0 && count > product.maxVotesPerIssue) {
      invalid(`at most ${product.maxVotesPerIssue} votes may be cast on one issue`);
    }

    return must(
      await db.transaction(async (tx) => {
        // The per-product budget counts this user's votes on OTHER issues in the
        // same product, so replacing an existing vote frees its own allowance
        // rather than counting twice.
        const mine = await readAllTx(tx.votes, { userId: actor.id });
        const existing = mine.find((row) => row.issueId === issueId) ?? null;
        const issueIds = mine.map((row) => row.issueId).filter((id) => id !== issueId);
        const otherIssues: IssueRow[] = [];
        for (const id of issueIds) {
          const row = await tx.issues.get(id);
          if (row) otherIssues.push(row);
        }
        const spentElsewhere = otherIssues
          .filter((other) => other.productId === issue.productId)
          .reduce((total, other) => {
            const row = mine.find((entry) => entry.issueId === other.id);
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
          await tx.votes.insert({ issueId, userId: actor.id, count });
        }

        // voteCount is SUM(count), not COUNT(*) -- a vote row carries a
        // quantity. Recomputed from the rows rather than incremented, so it
        // cannot drift away from them.
        const after = await readAllTx(tx.votes, { issueId });
        const total = after.reduce((sum, row) => sum + row.count, 0);

        const patch: DbPatch = { voteCount: total };

        // The status is re-read INSIDE the transaction. Deciding from the row
        // fetched before it opened is a lost update with teeth: a concurrent
        // issues.resolve moves the issue to RESOLVED/FIXED, this transaction still
        // sees the stale UNCONFIRMED, and writes status=CONFIRMED while
        // leaving resolution=FIXED. That pair is a state assertValidState
        // rejects, so every later changeStatus, resolve, reopen and
        // markDuplicate throws on the way in -- and since resolution can only
        // be cleared through those same transitions, no RPC can repair it. The
        // issue is wedged permanently.
        const current = await getTxRequired(tx.issues, issueId, "Issue");

        // Bugzilla's auto-confirm: enough votes turn an UNCONFIRMED issue into a
        // CONFIRMED one. Only from UNCONFIRMED -- votes never move an issue that
        // is already resolved.
        const confirms =
          product.votesToConfirm > 0 &&
          total >= product.votesToConfirm &&
          current.status === "UNCONFIRMED";
        if (confirms) {
          patch.status = "CONFIRMED";
          patch.isConfirmed = true;
        }
        const updated = await tx.issues.update(issueId, patch);
        if (!updated) notFound("Issue");

        await recordRelatedChange(
          tx.activities,
          issueId,
          actor.id,
          "votes",
          String(current.voteCount),
          String(total),
        );
        if (confirms) {
          await recordRelatedChange(
            tx.activities,
            issueId,
            actor.id,
            "status",
            "UNCONFIRMED",
            "CONFIRMED",
          );
        }
        return { issueId, count, voteCount: total, confirmed: confirms };
        // Serializable, like every other read-modify-write in this file.
        // Without it the recompute below read committed does NOT make the
        // counter safe: two concurrent voters each sum the votes without
        // seeing the other's uncommitted row, and the later update wins with a
        // total that omits one of them. The comment above claiming voteCount
        // "cannot drift away from them" was only true under this isolation.
        // The SQLite dev tier serialises writes, so no local test can show it;
        // it is a Postgres-only failure.
      }, { isolationLevel: "serializable" }),
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
    const issues = await readByIds(db.issues, rows.map((row) => row.issueId));
    const hidden = new Set(await hiddenIssueIds(user));
    const visible = await visibleProductIds(identity);
    const byId = new Map(issues.map((issue) => [issue.id, issue]));
    return rows
      .filter((row) => {
        const issue = byId.get(row.issueId);
        return issue && !hidden.has(issue.id) && visible.has(issue.productId);
      })
      .map((row) => ({ ...row, issue: normalizeIssueRow(byId.get(row.issueId)!) }));
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
// better than rolling back the issue change that caused it. It does mean a crash
// between commit and fanout drops the notification silently.
// ---------------------------------------------------------------------------

/**
 * Everyone who should hear about a change to this issue: assignee, reporter, QA
 * contact, the CC list, and the watchers of each of those -- minus the actor,
 * who already knows.
 *
 * Recipients are filtered by `canViewIssue`. Notifying someone about an issue they
 * cannot open would leak its summary in the notification title, which is
 * exactly what a restricted issue is hiding.
 */
async function notifyIssueChange(
  issue: IssueRow,
  actorId: string,
  title: string,
  body?: string,
): Promise<void> {
  // The WHOLE body is guarded, not just the insert.
  //
  // This runs after the caller's transaction has committed, and it makes five
  // more database reads (CC list, watchers, users, canViewIssue's own reads, the
  // unread recount). Only the insert error was swallowed, so a transient
  // failure in any of the others threw out of a mutation that had ALREADY
  // committed: comments.add would return 500 with the comment saved, and a
  // retry -- by a client or a person -- would file it a second time. A fanout
  // hiccup became duplicated user data, which is exactly what the old comment
  // promised could not happen.
  try {
    await fanOutIssueChange(issue, actorId, title, body);
  } catch (error) {
    console.warn(`notification fanout failed for issue ${issue.id}: ${String(error)}`);
  }
}

async function fanOutIssueChange(
  issue: IssueRow,
  actorId: string,
  title: string,
  body?: string,
): Promise<void> {
  const direct = new Set<string>(
    [issue.assigneeId, issue.reporterId, issue.qaContactId].filter(
      (id): id is string => Boolean(id),
    ),
  );
  for (const row of await readAll(db.issueCc, { issueId: issue.id })) direct.add(row.userId);

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
    if (!(await canViewIssue(issue, user))) continue;
    const inserted = await db.notifications.insert({
      userId: user.id,
      issueId: issue.id,
      kind: "issue_changed",
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
    // requireId first. Without it a missing watchedId reached the row lookup
    // and came back 500, so a caller that simply forgot the argument got
    // "internal error" instead of being told which argument was wrong -- and
    // a 500 sends you looking at the server rather than the call.
    const watched = await getRequired(db.users, requireId(watchedId, "watchedId"), "User");
    if (watched.id === actor.id) invalid("you cannot watch yourself");
    // readAll, not .get with a composite filter. That form returned null for
    // a row that existed, so the idempotency guard never fired and watching
    // someone you already watch hit the (watcherId, watchedId) unique index
    // and came back 500 -- "internal error" for an action whose correct
    // answer is "you already do".
    const [existing] = await readAll(db.watchers, {
      watcherId: actor.id,
      watchedId: watched.id,
    });
    if (existing) return existing;
    // Clear any tombstone for this pair first. readAll cannot see a
    // soft-deleted row but the unique index can, so without this an insert
    // after an old-style delete fails forever and self-heals for nobody.
    await db.watchers.purgeMany({ watcherId: actor.id, watchedId: watched.id });
    return must(await db.watchers.insert({ watcherId: actor.id, watchedId: watched.id }));
  },
  { id: "watchers.add" },
);

export const removeWatcher = mutation(
  async ({ watchedId }: { watchedId: string }) => {
    const actor = await requireActor();
    const [existing] = await readAll(db.watchers, {
      watcherId: actor.id,
      watchedId: requireId(watchedId, "watchedId"),
    });
    if (!existing) notFound("Watch");
    // PURGE, not delete. delete is a soft delete: it stamps deleted_at and
    // leaves the row, which the (watcherId, watchedId) unique index still
    // counts while readAll no longer returns it. So unwatching someone made
    // them permanently unwatchable -- the guard could not see the row and the
    // insert hit the index, answering "internal error" forever after.
    //
    // A join row carries no history worth keeping: the fact that you once
    // watched somebody is not data this app reports on.
    must(await db.watchers.purge(existing.id));
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
// Bugzilla's cross-tracker links: an issue in another system that is the same
// issue, or related to it. The table existed with zero server references, so
// the schema described the feature and nothing implemented it.
//
// Stored as a plain URL rather than a parsed reference: the whole point is
// pointing at trackers this app knows nothing about.
// ---------------------------------------------------------------------------

export const addSeeAlso = mutation(
  async ({ issueId, url }: { issueId: string; url: string }) => {
    const actor = await requireActor();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueAccessible(issue, actor);

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

    const existing = must(await db.issueSeeAlso.get({ issueId, url: clean }));
    if (existing) return existing;
    return must(
      await db.transaction(async (tx) => {
        const row = await tx.issueSeeAlso.insert({ issueId, url: clean });
        await recordRelatedChange(tx.activities, issueId, actor.id, "see_also", null, clean);
        return row;
        // Serializable for the same reason as cc.add: idempotent
        // get-then-insert. Note the existence check for this one sits OUTSIDE
        // the transaction, so it was doubly racy.
      }, { isolationLevel: "serializable" }),
    );
  },
  { id: "seeAlso.add" },
);

export const removeSeeAlso = mutation(
  async ({ id }: { id: string }) => {
    const actor = await requireActor();
    const row = must(await db.issueSeeAlso.get(requireId(id, "id")));
    if (!row) notFound("See Also link");
    const issue = await getRequired(db.issues, row.issueId, "Issue");
    await assertIssueAccessible(issue, actor);
    return must(
      await db.transaction(async (tx) => {
        await tx.issueSeeAlso.delete(row.id);
        await recordRelatedChange(tx.activities, row.issueId, actor.id, "see_also", row.url, null);
        return { removed: true };
      }),
    );
  },
  { id: "seeAlso.remove" },
);

export const listSeeAlso = query(
  async ({ issueId }: { issueId: string }) => {
    const identity = requireIdentity();
    const issue = await getRequired(db.issues, issueId, "Issue");
    await assertIssueVisible(issue, identity);
    return readAll(db.issueSeeAlso, { issueId });
  },
  { id: "seeAlso.list" },
);
