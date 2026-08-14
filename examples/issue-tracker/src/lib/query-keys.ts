/**
 * The cache's vocabulary: one place that decides what is keyed by what.
 *
 * Written ahead of the TanStack Query migration, because the key shape is the
 * part that is expensive to get wrong. Once 32 call sites are keyed, changing
 * the taxonomy means touching all of them again, while the fetching code is
 * mechanical either way.
 *
 * WHY THIS EXISTS AT ALL. The app had no cache, and paid for it in two ways
 * that were easy to count. Both are now fixed, and the numbers are kept
 * because they are what justified the change:
 *
 *   - The same fact was fetched repeatedly. Six components called `users.me`
 *     independently, so one visit to the issue list fired it FOUR times, and
 *     `products.resolve` and `users.resolve` went out SIX times each. The
 *     same page now makes 6 requests in total with no duplicates.
 *
 *   - Freshness was threaded by hand: ~124 `reload()` / `onUpdated` /
 *     `onChanged` props existed so a mutation in one panel could tell its
 *     siblings they were stale. Zero remain. A mutation now names what it
 *     invalidated and does not need to know who was listening.
 *
 * SHAPE. Keys are arrays, ordered widest to narrowest, so a prefix
 * invalidation catches everything beneath it:
 *
 *     issues.all              ["issues"]
 *     issues.list(filters)    ["issues", "list", {...}]
 *     issues.detail(id)       ["issues", "detail", id]
 *
 * `invalidateQueries({ queryKey: issues.all })` then drops both the list and
 * every detail, while `issues.detail(id)` drops exactly one. Getting that
 * gradient right is the whole point: blunt invalidation refetches the world on
 * every keystroke, and over-precise invalidation is how a stale row survives a
 * mutation that changed it.
 *
 * ARGUMENTS ARE PART OF THE KEY. `reports.summary(productId)` and
 * `reports.summary(null)` are different questions with different answers, so
 * they are different entries. A key that dropped the argument would serve one
 * product's numbers for another, which is the failure mode a cache introduces
 * and a cacheless app cannot have.
 */

/** Filters the issue list sends to `issues.search`. Part of the key, so two
 *  different filter sets do not share an entry. */
export type IssueListKeyInput = Readonly<Record<string, unknown>>;

export const queryKeys = {
  users: {
    all: ["users"] as const,
    /** The signed-in identity. ONE entry, shared by every consumer -- this is
     *  the key that makes the four-times fetch impossible rather than merely
     *  discouraged. */
    me: () => ["users", "me"] as const,
    resolve: (ids: readonly string[]) => ["users", "resolve", [...ids].sort()] as const,
  },

  issues: {
    all: ["issues"] as const,
    list: (filters: IssueListKeyInput) => ["issues", "list", filters] as const,
    detail: (id: string) => ["issues", "detail", id] as const,
  },

  comments: {
    all: ["comments"] as const,
    list: (issueId: string) => ["comments", "list", issueId] as const,
  },

  attachments: {
    all: ["attachments"] as const,
    list: (issueId: string) => ["attachments", "list", issueId] as const,
  },

  /** Relations hanging off one issue. Grouped so a single issue's side panels
   *  can be invalidated together when the issue itself changes. */
  relations: {
    all: ["relations"] as const,
    cc: (issueId: string) => ["relations", "cc", issueId] as const,
    ccMine: () => ["relations", "cc", "mine"] as const,
    dependencies: (issueId: string) => ["relations", "deps", issueId] as const,
    duplicates: (issueId: string) => ["relations", "dupes", issueId] as const,
    seeAlso: (issueId: string) => ["relations", "seeAlso", issueId] as const,
    keywords: (issueId: string) => ["relations", "keywords", issueId] as const,
    flags: (issueId: string) => ["relations", "flags", issueId] as const,
    votes: (issueId: string) => ["relations", "votes", issueId] as const,
  },

  products: {
    all: ["products"] as const,
    /** Five callers today, each fetching the whole list independently. */
    list: () => ["products", "list"] as const,
    /**
     * Id-to-label resolution for the rows currently on screen.
     *
     * Sorted, because the ID SET is the identity of this question and
     * [a, b] must not be a different cache entry from [b, a]. The table
     * re-derives its id list on every render; without the sort, scrolling
     * would miss the cache constantly.
     */
    resolve: (ids: readonly string[]) => ["products", "resolve", [...ids].sort()] as const,
    detail: (id: string) => ["products", "detail", id] as const,
  },

  keywords: { all: ["keywords"] as const, list: () => ["keywords", "list"] as const },

  notifications: {
    all: ["notifications"] as const,
    list: () => ["notifications", "list"] as const,
    /**
     * Separate from `list` because the dashboard renders the list while the
     * header shows only a count; marking one read must drop both, which the
     * shared `notifications` prefix gives us.
     *
     * THIS COMMENT USED TO SAY "the header badge polls it", which was a
     * description of the intended end state and not of the code. `UnreadBadge`
     * was a `useState` + `useEffect` calling the procedure directly, with
     * `[signedIn]` as its only dependency, so nothing subscribed to this key
     * at all: invalidation dropped an entry no component was reading, and the
     * badge went stale until the next sign-in or full remount.
     *
     * It is the rule in `queries.ts` -- nothing fetches outside a hook --
     * demonstrated by the one component that broke it. A bespoke fetch is
     * invisible to the cache, and looks right until something else changes
     * the data.
     */
    unreadCount: () => ["notifications", "unreadCount"] as const,
  },

  savedSearches: { all: ["savedSearches"] as const, list: () => ["savedSearches", "list"] as const },

  flags: { all: ["flags"] as const, requests: () => ["flags", "requests"] as const },

  groups: { all: ["groups"] as const, list: () => ["groups", "list"] as const },

  reports: {
    all: ["reports"] as const,
    summary: (productId: string | null) => ["reports", "summary", productId] as const,
    byComponent: (productId: string | null) => ["reports", "byComponent", productId] as const,
  },
} as const;

/**
 * What a mutation makes stale.
 *
 * Kept BESIDE the keys rather than at each call site, because the whole defect
 * being fixed is that this knowledge was scattered: a panel had to be handed a
 * callback that knew which sibling to refresh. Written as prefixes, so adding
 * a query under an existing prefix is covered without editing this.
 *
 * The reports entry is the one worth reading twice. Every issue mutation
 * changes the numbers, and it is the invalidation people forget -- a report
 * that silently lags the data it summarises is worse than one that is slow,
 * because nothing about it looks wrong.
 */
export const invalidatedBy = {
  /** Anything that edits an issue's own fields. */
  issueChanged: (issueId: string) => [
    queryKeys.issues.detail(issueId),
    queryKeys.issues.all,
    queryKeys.reports.all,
  ],
  /** Filing a new issue: no detail to drop, but the list and the numbers move. */
  issueCreated: () => [queryKeys.issues.all, queryKeys.reports.all],
  /** A comment changes `commentCount` on the issue too, so the detail goes as
   *  well as the thread. Missing that is how a comment appears while the count
   *  beside it still reads one fewer. */
  commentChanged: (issueId: string) => [
    queryKeys.comments.list(issueId),
    queryKeys.issues.detail(issueId),
  ],
  attachmentChanged: (issueId: string) => [
    queryKeys.attachments.list(issueId),
    queryKeys.issues.detail(issueId),
  ],
  /** A flag write changes both the live flags on an issue and the account
   *  request list when its status is `?`. */
  flagChanged: (issueId: string) => [
    queryKeys.relations.flags(issueId),
    queryKeys.flags.all,
  ],
  /** Casting zero or more votes adds, changes or removes the account row. */
  voteChanged: () => [queryKeys.relations.votes("mine")],
  /** Every side panel of one issue, for mutations whose blast radius is the
   *  relation graph rather than one list. */
  relationsChanged: (issueId: string) => [
    queryKeys.relations.all,
    queryKeys.issues.detail(issueId),
  ],
  productStructureChanged: () => [queryKeys.products.all, queryKeys.reports.all],
  /**
   * Creating a keyword changes the tracker-wide VOCABULARY, not just this
   * issue, so it drops the keyword list as well as the issue's relations.
   *
   * Named here because two separate agents independently composed this same
   * pair at their call sites when the entry was missing. That is the signal
   * that a shape belongs in the map: if the answer has to be re-derived, it
   * will eventually be re-derived differently.
   */
  keywordCreated: (issueId: string) => [
    queryKeys.relations.keywords(issueId),
    queryKeys.issues.detail(issueId),
    queryKeys.keywords.all,
  ],
  savedSearchChanged: () => [queryKeys.savedSearches.all],
  notificationsChanged: () => [queryKeys.notifications.all],
} as const;
