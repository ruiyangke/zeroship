import { useMutation, useQuery, useQueryClient, type QueryKey } from "@tanstack/react-query";

import {
  currentUser,
  dependencyGraph,
  getAttachment,
  getIssue,
  getProduct,
  listAttachments,
  listCc,
  listComments,
  listDuplicates,
  listFlags,
  listFlagRequests,
  listGroups,
  listKeywords,
  listMyCc,
  listMyVotes,
  listNotifications,
  listProducts,
  listSavedSearches,
  unreadNotificationCount,
  reportByAssignee,
  reportByComponent,
  reportSummary,
  resolveProducts,
  resolveUsers,
  reportTimeToResolve,
  reportTrend,
  searchIssues,
} from "../api";
import { toPromise } from "../components/rpc";
import type { FlagRequestEntry, IssueDetail } from "../components/types";
import { queryKeys } from "./query-keys";

/**
 * Every read this app makes, in one place.
 *
 * THE RULE, and the reason this file exists: no component calls `useQuery`
 * directly and nothing calls a procedure outside a hook here. Before this the
 * app had its own `useAsync` -- "no cache, no refetch-on-focus, no request
 * de-duplication", by its own description -- invoked at 32 sites, and the
 * consequences were all the same shape:
 *
 *   - The same fact was fetched per component rather than per app. Measured on
 *     one issue-list load: `users.me` x4, `products.resolve` x6,
 *     `users.resolve` x6, `products.list` x4.
 *   - Freshness was threaded by hand. ~124 `reload()` / `onUpdated` /
 *     `onChanged` props existed so a mutating panel could tell its siblings
 *     they were stale, which meant every new panel had to be wired in by
 *     someone who remembered. None remain: the last four panels
 *     (fields, flags, votes, security) were the final holdouts.
 *
 * Both are properties of having no shared cache, so both are fixed by having
 * one. Keeping the hooks together is what stops the second cache appearing:
 * a component that calls a procedure itself is invisible to invalidation, and
 * looks correct right up until something else changes the data.
 *
 * ARGUMENTS BELONG IN THE KEY. `listProducts({})` and
 * `listProducts({ includeInactive: true })` are different questions with
 * different answers; a key that dropped the argument would serve one for the
 * other. That is the failure mode a cache introduces and a cacheless app
 * cannot have, so every hook below threads its input into the key.
 */

/** Options every list hook accepts, so a caller can defer a dependent query. */
type Gate = { enabled?: boolean };

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

export function useCurrentUser() {
  return useQuery({ queryKey: queryKeys.users.me(), queryFn: () => currentUser({}) });
}

// ---------------------------------------------------------------------------
// Issues
// ---------------------------------------------------------------------------

export type IssueSearchInput = Parameters<typeof searchIssues>[0];

export function useIssueSearch(input: IssueSearchInput, gate: Gate = {}) {
  return useQuery({
    queryKey: queryKeys.issues.list(input as Record<string, unknown>),
    queryFn: () => searchIssues(input),
    /**
     * The list KEEPS the rows it has while the next page or filter loads.
     *
     * This preserves a behaviour `useAsync` had earned the hard way: its
     * comment records that dropping to a loading state on every refetch
     * "blanked the section and jumped the layout" on each filter change, sort
     * or page. `placeholderData` is the query-cache spelling of that, and it
     * is opt-in, so it is stated here rather than assumed.
     */
    placeholderData: (previous) => previous,
    enabled: gate.enabled,
  });
}

export function useIssue(id: string, gate: Gate = {}) {
  return useQuery({
    queryKey: queryKeys.issues.detail(id),
    queryFn: () => getIssue({ id }),
    enabled: gate.enabled ?? Boolean(id),
  });
}

// ---------------------------------------------------------------------------
// One issue's thread and relations
// ---------------------------------------------------------------------------

export function useComments(issueId: string) {
  return useQuery({
    queryKey: queryKeys.comments.list(issueId),
    queryFn: () => listComments({ issueId }),
    enabled: Boolean(issueId),
  });
}

export function useAttachments(issueId: string) {
  return useQuery({
    queryKey: queryKeys.attachments.list(issueId),
    queryFn: () => listAttachments({ issueId }),
    enabled: Boolean(issueId),
  });
}

export function useCc(issueId: string) {
  return useQuery({
    queryKey: queryKeys.relations.cc(issueId),
    queryFn: () => listCc({ issueId }),
    enabled: Boolean(issueId),
  });
}

export function useDependencyGraph(issueId: string) {
  return useQuery({
    queryKey: queryKeys.relations.dependencies(issueId),
    queryFn: () => dependencyGraph({ issueId }),
    enabled: Boolean(issueId),
  });
}

export function useDuplicates(issueId: string) {
  return useQuery({
    queryKey: queryKeys.relations.duplicates(issueId),
    queryFn: () => listDuplicates({ issueId }),
    enabled: Boolean(issueId),
  });
}

export function useFlags(issueId: string) {
  return useQuery({
    queryKey: queryKeys.relations.flags(issueId),
    queryFn: () => listFlags({ issueId }),
    enabled: Boolean(issueId),
  });
}

// ---------------------------------------------------------------------------
// Account-scoped lists
// ---------------------------------------------------------------------------

export function useMyCc() {
  return useQuery({ queryKey: queryKeys.relations.ccMine(), queryFn: () => listMyCc({}) });
}

export function useMyVotes() {
  return useQuery({ queryKey: queryKeys.relations.votes("mine"), queryFn: () => listMyVotes({}) });
}

export function useFlagRequests() {
  return useQuery({ queryKey: queryKeys.flags.requests(), queryFn: () => listFlagRequests({}) });
}

export function useGroups() {
  return useQuery({ queryKey: queryKeys.groups.list(), queryFn: () => listGroups({}) });
}

/** One flag request row, once the issue behind it is known. */
export type FlagRequestRow = { entry: FlagRequestEntry; issue: IssueDetail["issue"] | null };

/**
 * A flag names an issue OR an attachment, and an attachment has to be resolved
 * to the issue it hangs off before a row can link anywhere. A failure to
 * resolve is a null issue, not a failed row: the flag itself is still worth
 * listing, and the row falls back to saying it is on an attachment.
 */
async function flagIssueId(entry: FlagRequestEntry): Promise<string | null> {
  if (entry.flag.issueId) return entry.flag.issueId;
  if (!entry.flag.attachmentId) return null;
  try {
    const { attachment } = await getAttachment({ id: entry.flag.attachmentId });
    return attachment.issueId;
  } catch {
    return null;
  }
}

/**
 * The issues a set of flag requests points at.
 *
 * A hook rather than a fetch inside the dashboard, even though it is two hops
 * over other procedures rather than one call: a component that fetches on its
 * own is invisible to invalidation, which is exactly what this file exists to
 * prevent.
 *
 * KEYED BY THE FLAG IDS, not by the entries array. `listFlagRequests` hands
 * back a fresh array every fetch, so keying on identity would refetch on each
 * one; the answer only moves when the set of flags does. The key sits under
 * the `flags.requests` prefix so `flags.all` drops the resolved issues along
 * with the list they were derived from.
 *
 * Only the RESOLUTION is cached -- flag id to issue. The entry itself is
 * zipped back on in `select`, from the props the caller holds right now, so a
 * flag whose status changed under an unchanged id cannot be rendered from a
 * cached copy of its old self. Caching the joined row would make the key a
 * promise it does not keep: the ids are the same, the entries are not.
 */
export function useFlagRequestIssues(entries: readonly FlagRequestEntry[]) {
  const flagIds = entries.map((entry) => entry.flag.id).sort();
  return useQuery({
    queryKey: [...queryKeys.flags.requests(), "issues", flagIds] as QueryKey,
    queryFn: async (): Promise<Record<string, IssueDetail["issue"] | null>> => {
      const withIssueId = await Promise.all(
        entries.map(async (entry) => ({ flagId: entry.flag.id, issueId: await flagIssueId(entry) })),
      );
      const issueIds = [
        ...new Set(withIssueId.map((x) => x.issueId).filter((id): id is string => id !== null)),
      ];
      const issues = await Promise.all(
        issueIds.map((id) => toPromise(getIssue({ id })).catch(() => null)),
      );
      const byId = new Map<string, IssueDetail["issue"]>();
      for (const detail of issues) {
        if (detail) byId.set(detail.issue.id, detail.issue);
      }
      return Object.fromEntries(
        withIssueId.map(({ flagId, issueId }) => [
          flagId,
          issueId ? byId.get(issueId) ?? null : null,
        ]),
      );
    },
    select: (byFlagId): FlagRequestRow[] =>
      entries.map((entry) => ({ entry, issue: byFlagId[entry.flag.id] ?? null })),
  });
}

export function useNotifications(limit = 50) {
  return useQuery({
    queryKey: [...queryKeys.notifications.list(), limit] as QueryKey,
    queryFn: () => listNotifications({ limit }),
  });
}

/**
 * The header's unread count.
 *
 * Its own key rather than a projection of the list, because the header wants a
 * number and the dashboard wants fifty rows; sharing an entry would make the
 * badge pay for the list on every page.
 *
 * This hook exists because the badge was the one component still fetching
 * outside the cache -- a `useState` + `useEffect` keyed on `[signedIn]`. The
 * mark-read mutation correctly invalidated `notifications.unreadCount`, but
 * nothing subscribed to that key, so the badge kept its number until the next
 * sign-in. That is precisely the failure the "no fetching outside a hook" rule
 * prevents, and it survived until someone checked the badge rather than the
 * invalidation.
 */
export function useUnreadNotificationCount(gate: Gate = {}) {
  return useQuery({
    queryKey: queryKeys.notifications.unreadCount(),
    queryFn: () => unreadNotificationCount({}),
    enabled: gate.enabled,
  });
}

export function useSavedSearches() {
  return useQuery({
    queryKey: queryKeys.savedSearches.list(),
    queryFn: () => listSavedSearches({}),
  });
}

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

export type ProductListInput = Parameters<typeof listProducts>[0];

/** Five call sites before the cache; one entry per distinct input after it. */
export function useProducts(input: ProductListInput = {}) {
  return useQuery({
    queryKey: [...queryKeys.products.list(), input] as QueryKey,
    queryFn: () => listProducts(input),
  });
}

export function useProduct(id: string | null) {
  return useQuery({
    queryKey: queryKeys.products.detail(id ?? ""),
    queryFn: () => getProduct({ id: id! }),
    enabled: Boolean(id),
  });
}

/**
 * Resolve the product and user ids visible in a table, keyed by the id SET.
 *
 * These two were the last readers left on the old `useAsync`, and the most
 * expensive: measured on one issue-list load, `products.resolve` and
 * `users.resolve` each went out SIX times, because every table and panel
 * rendering rows resolved its own labels. The id set is usually identical or
 * overlapping between them, so those were six answers to nearly the same
 * question.
 *
 * An empty id list is not asked at all. `enabled` keeps a table that has not
 * loaded its rows yet from firing a resolve for nothing, which is also what
 * stops a burst of empty calls during the first render pass.
 */
export function useResolvedProducts(ids: readonly string[]) {
  return useQuery({
    queryKey: queryKeys.products.resolve(ids),
    queryFn: () => resolveProducts({ ids: [...ids] }),
    enabled: ids.length > 0,
  });
}

export function useResolvedUsers(ids: readonly string[]) {
  return useQuery({
    queryKey: queryKeys.users.resolve(ids),
    queryFn: () => resolveUsers({ ids: [...ids] }),
    enabled: ids.length > 0,
  });
}

export function useKeywords() {
  return useQuery({ queryKey: queryKeys.keywords.list(), queryFn: () => listKeywords({}) });
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

export function useReportSummary(productId: string | null) {
  return useQuery({
    queryKey: queryKeys.reports.summary(productId),
    queryFn: () => reportSummary({ productId: productId || undefined }),
  });
}

export function useReportByComponent(productId: string | null) {
  return useQuery({
    queryKey: queryKeys.reports.byComponent(productId),
    queryFn: () => reportByComponent({ productId: productId || undefined }),
  });
}

/**
 * The three reports the taxonomy does not name yet.
 *
 * `query-keys.ts` spells out `reports.summary` and `reports.byComponent`; the
 * assignee, trend and time-to-resolve reports compose their keys here instead,
 * the same way `useProducts` and `useNotifications` compose an argument onto a
 * named key. They stay UNDER the `["reports"]` prefix, which is the part that
 * matters: `invalidatedBy.issueChanged` drops `reports.all`, so these three go
 * stale with every other number on the page rather than being the ones nobody
 * remembered. Promote them into the taxonomy if that file grows a section.
 *
 * The window is part of the key as much as the product is. A 7-day trend and a
 * 90-day trend are different questions, and a key that dropped `days` would
 * serve one for the other on the next window change.
 */
export function useReportByAssignee(productId: string | null) {
  return useQuery({
    queryKey: [...queryKeys.reports.all, "byAssignee", productId] as QueryKey,
    queryFn: () => reportByAssignee({ productId: productId || undefined }),
  });
}

export function useReportTrend(productId: string | null, days: number) {
  return useQuery({
    queryKey: [...queryKeys.reports.all, "trend", productId, days] as QueryKey,
    queryFn: () => reportTrend({ productId: productId || undefined, days }),
  });
}

export function useReportTimeToResolve(productId: string | null, days: number) {
  return useQuery({
    queryKey: [...queryKeys.reports.all, "timeToResolve", productId, days] as QueryKey,
    queryFn: () => reportTimeToResolve({ productId: productId || undefined, days }),
  });
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/**
 * Run a mutation and drop what it made stale.
 *
 * The `invalidates` argument takes the key prefixes from `invalidatedBy` in
 * query-keys.ts, so the blast radius of a write is declared next to the keys
 * rather than rediscovered at each call site. That is the knowledge the ~124
 * threaded callbacks were carrying, badly: a panel could only refresh a
 * sibling it had been handed a callback for, so the ones nobody remembered --
 * the report totals, most often -- silently lagged.
 *
 * Invalidation is AWAITED before the mutation resolves, so a caller that
 * closes a dialog on success closes it over refreshed data rather than racing
 * it.
 */
export function useAppMutation<TArgs, TResult>(
  run: (args: TArgs) => Promise<TResult> | TResult,
  invalidates: (args: TArgs, result: TResult) => readonly QueryKey[],
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (args: TArgs) => await run(args),
    /**
     * `onSettled`, NOT `onSuccess`.
     *
     * A mutation that throws may still have changed the server. The clearest
     * case in this app is posting a comment with a file: `addComment`
     * succeeds, `uploadAttachment` then fails, and the mutation rejects -- so
     * with `onSuccess` nothing is invalidated and the comment sits on the
     * server while the thread and the count both keep showing the old world
     * until something unrelated moves. The old callback chain had the same
     * hole for the same reason (`onAdded()` was never reached).
     *
     * Refetching after a failure costs one request and cannot show anything
     * false; skipping it can leave the screen disagreeing with the database,
     * which is the failure this whole layer exists to prevent.
     */
    onSettled: async (result, _error, args) => {
      await Promise.all(
        invalidates(args, result as TResult).map((queryKey) =>
          queryClient.invalidateQueries({ queryKey }),
        ),
      );
    },
  });
}

/** Imperative invalidation, for the few places that are not a mutation --
 *  a sign-in, or a panel that knows a sibling's data moved underneath it. */
export function useInvalidate() {
  const queryClient = useQueryClient();
  return (keys: readonly QueryKey[]) =>
    Promise.all(keys.map((queryKey) => queryClient.invalidateQueries({ queryKey })));
}
