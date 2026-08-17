/**
 * The database rows, as this server sees them.
 *
 * Extracted from index.ts, which had grown to 4779 lines with the first
 * procedure at line 1096 -- a fifth of the file was the schema restated in
 * TypeScript, sitting between the imports and any code that does something.
 *
 * TYPES ONLY, deliberately. The build discovers RPC procedures by analysing
 * the server entry and rewriting it for the client, and a procedure moved into
 * another module does not survive that: it works in `pnpm dev`, vanishes from
 * the deployed bundle if the client never imports it, and fails the build with
 * MISSING_EXPORT if it does. Measured 2026-08-14. Types erase at compile time,
 * so they can live here; procedures cannot, and that is why index.ts is still
 * large.
 */

export type EmptyInput = Record<string, never>;
export type DbFilter = Record<string, unknown>;
export type DbPatch = Record<string, unknown>;
export type DbResult<T> =
  | { data: T; error: null }
  | { data: null; error: Error };

export type SystemRow = {
  id: string;
  created_at: number;
  updated_at: number;
  created_by: string | null;
  updated_by: string | null;
  version: number;
  deleted_at: number | null;
};

export type UserRow = SystemRow & {
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

export type ProductRow = SystemRow & {
  name: string;
  key: string;
  description?: string | null;
  classification: string;
  defaultMilestone?: string | null;
  allowsUnconfirmed: boolean;
  isActive: boolean;
  // Bugzilla voting limits. Present in the schema since the rework and
  // absent from this view, which is why nothing could read them.
  votesPerUser: number;
  maxVotesPerIssue: number;
  votesToConfirm: number;
};

export type ComponentRow = SystemRow & {
  productId: string;
  name: string;
  description?: string | null;
  defaultAssigneeId?: string | null;
  defaultQaContactId?: string | null;
  isActive: boolean;
};

export type VersionRow = SystemRow & {
  productId: string;
  name: string;
  sortKey: number;
  isActive: boolean;
};

export type MilestoneRow = VersionRow;

export type IssueRow = SystemRow & {
  productId: string;
  number: number;
  componentId: string;
  // NULLABLE in the schema, and optional here for the same reason: "version
  // found in" is a defect concept, and an enhancement has none.
  versionId?: string | null;
  milestoneId?: string | null;
  summary: string;
  // What the row IS -- "defect" | "enhancement" | "task" -- kept apart from
  // how bad it is. NOT NULL, default "defect". Typed as `string` like every
  // other closed-vocabulary column here: the row type mirrors what the
  // database returns, and the narrow union lives in ./lib/quicksearch.
  kind: string;
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
  deadline?: number | null;
  // Stamped by issues.resolve / issues.markDuplicate and cleared by issues.reopen.
  // reports.timeToResolve reads this instead of mining the activities table
  // for status->RESOLVED transitions, which was a scan plus string matching.
  resolvedAt?: number | null;
};

export type CommentRow = SystemRow & {
  issueId: string;
  authorId: string;
  body: string;
  commentNumber: number;
  isPrivate: boolean;
};

export type AttachmentRow = SystemRow & {
  issueId: string;
  commentId?: string | null;
  uploaderId: string;
  filename: string;
  contentType: string;
  sizeBytes: number;
  storageKey: string;
  description?: string | null;
  isPatch: boolean;
  isObsolete: boolean;
};

export type KeywordRow = SystemRow & {
  name: string;
  description?: string | null;
};

export type IssueKeywordRow = SystemRow & { issueId: string; keywordId: string };
export type DependencyRow = SystemRow & { issueId: string; dependsOnId: string };
export type CcRow = SystemRow & { issueId: string; userId: string };
export type SeeAlsoRow = SystemRow & { issueId: string; url: string };
export type WatcherRow = SystemRow & { watcherId: string; watchedId: string };
export type VoteRow = SystemRow & { issueId: string; userId: string; count: number };
export type ProductGroupRow = SystemRow & { productId: string; groupId: string };
export type GroupMemberRow = SystemRow & { groupId: string; userId: string };
export type GroupRow = SystemRow & {
  name: string;
  description?: string | null;
};
export type IssueGroupRow = SystemRow & { issueId: string; groupId: string };

export type FlagTypeRow = SystemRow & {
  name: string;
  description?: string | null;
  targetType: string;
  isRequestable: boolean;
  isMultiplicable: boolean;
  productId?: string | null;
};

export type FlagRow = SystemRow & {
  flagTypeId: string;
  issueId?: string | null;
  attachmentId?: string | null;
  setterId: string;
  requesteeId?: string | null;
  status: string;
};

export type ActivityRow = SystemRow & {
  issueId: string;
  actorId: string;
  fieldName: string;
  oldValue?: string | null;
  newValue?: string | null;
};

export type SavedSearchRow = SystemRow & {
  ownerId: string;
  name: string;
  queryJson: unknown;
  isShared: boolean;
};

export type NotificationRow = SystemRow & {
  userId: string;
  issueId?: string | null;
  kind: string;
  title: string;
  body?: string | null;
  isRead: boolean;
};

export interface ReadQuery<Row> extends PromiseLike<DbResult<Row[]>> {
  sort(order: Record<string, 1 | -1>): ReadQuery<Row>;
  limit(count: number): ReadQuery<Row>;
  skip(count: number): ReadQuery<Row>;
  after(id: string): ReadQuery<Row>;
  first(): Promise<DbResult<Row | null>>;
}

export interface Collection<Row> {
  get(idOrFilter: string | DbFilter): Promise<DbResult<Row | null>>;
  find(filter?: DbFilter): ReadQuery<Row>;
  insert(row: Record<string, unknown>): Promise<DbResult<Row>>;
  insertMany(rows: Record<string, unknown>[]): Promise<DbResult<Row[]>>;
  update(idOrFilter: string | DbFilter, patch: DbPatch): Promise<DbResult<Row | null>>;
  delete(idOrFilter: string | DbFilter): Promise<DbResult<Row | null>>;
  deleteMany(filter: DbFilter): Promise<DbResult<{ deletedCount: number }>>;
  // HARD delete. `delete` stamps deleted_at and leaves the row, which a
  // unique index still counts -- so a join row removed with `delete` can
  // never be recreated. This interface is a hand-written mirror of the SDK
  // Collection and omitted purge entirely, so the method the runtime has read
  // as one that does not exist.
  purge(idOrFilter: string | DbFilter): Promise<DbResult<Row | null>>;
  purgeMany(filter: DbFilter): Promise<DbResult<{ deletedCount: number }>>;
  count(filter?: DbFilter): Promise<DbResult<number>>;
}

export interface TxQuery<Row> extends PromiseLike<Row[]> {
  sort(order: Record<string, 1 | -1>): TxQuery<Row>;
  limit(count: number): TxQuery<Row>;
  skip(count: number): TxQuery<Row>;
  after(id: string): TxQuery<Row>;
  first(): Promise<Row | null>;
}

export interface TxCollection<Row> {
  get(idOrFilter: string | DbFilter): Promise<Row | null>;
  find(filter?: DbFilter): TxQuery<Row>;
  insert(row: Record<string, unknown>): Promise<Row>;
  insertMany(rows: Record<string, unknown>[]): Promise<Row[]>;
  update(idOrFilter: string | DbFilter, patch: DbPatch): Promise<Row | null>;
  delete(idOrFilter: string | DbFilter): Promise<Row | null>;
  deleteMany(filter: DbFilter): Promise<{ deletedCount: number }>;
  count(filter?: DbFilter): Promise<number>;
}

export type TxDb = {
  users: TxCollection<UserRow>;
  products: TxCollection<ProductRow>;
  components: TxCollection<ComponentRow>;
  versions: TxCollection<VersionRow>;
  milestones: TxCollection<MilestoneRow>;
  issues: TxCollection<IssueRow>;
  comments: TxCollection<CommentRow>;
  attachments: TxCollection<AttachmentRow>;
  keywords: TxCollection<KeywordRow>;
  issueKeywords: TxCollection<IssueKeywordRow>;
  issueDependencies: TxCollection<DependencyRow>;
  issueCc: TxCollection<CcRow>;
  votes: TxCollection<VoteRow>;
  issueSeeAlso: TxCollection<SeeAlsoRow>;
  issueGroups: TxCollection<IssueGroupRow>;
  flags: TxCollection<FlagRow>;
  activities: TxCollection<ActivityRow>;
  savedSearches: TxCollection<SavedSearchRow>;
  notifications: TxCollection<NotificationRow>;
};

export type AppDb = {
  users: Collection<UserRow>;
  products: Collection<ProductRow>;
  components: Collection<ComponentRow>;
  versions: Collection<VersionRow>;
  milestones: Collection<MilestoneRow>;
  groups: Collection<GroupRow>;
  productGroups: Collection<ProductGroupRow>;
  groupMembers: Collection<GroupMemberRow>;
  issueGroups: Collection<IssueGroupRow>;
  issues: Collection<IssueRow>;
  // Every table the migration creates is now listed here. This comment used to
  // say issueSeeAlso, votes and watchers were absent and their features
  // unimplemented; all three are listed above and implemented.
  comments: Collection<CommentRow>;
  attachments: Collection<AttachmentRow>;
  keywords: Collection<KeywordRow>;
  issueKeywords: Collection<IssueKeywordRow>;
  issueDependencies: Collection<DependencyRow>;
  issueCc: Collection<CcRow>;
  votes: Collection<VoteRow>;
  watchers: Collection<WatcherRow>;
  issueSeeAlso: Collection<SeeAlsoRow>;
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
