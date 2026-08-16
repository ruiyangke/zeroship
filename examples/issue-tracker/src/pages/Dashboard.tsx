import { useMemo } from "react";
import { Link } from "react-router-dom";
import { Badge, Grid, PageHeader, Tabs } from "@zeroship/ui";
import { ALL_ISSUE_COLUMNS, IssueResultsTable } from "../components/IssueResultsTable";
import { DashboardSection } from "../components/DashboardSection";
import { NotificationsPanel } from "../components/NotificationsPanel";
import { WatchingPanel } from "../components/WatchingPanel";
import { AsyncSection, SignInRequired, type QueryLike } from "../components/StateViews";
import { RequireSession, isSignedIn, useSession } from "../components/session";
import {
  useFlagRequestIssues,
  useFlagRequests,
  useIssueSearch,
  useMyCc,
  useMyVotes,
} from "../lib/queries";
import type { Issue, FlagRequestEntry } from "../components/types";
import { Hint, Muted, Page, SectionHeading } from "../components/AppPrimitives";

const COLUMNS = ALL_ISSUE_COLUMNS.map((c) => c.key).filter((c) => c !== "reporter");

/**
 * One issue table, four ways in.
 *
 * The four account-scoped queries -- assigned, reported, CC'd, voted for -- were
 * four stacked sections, each a full table of up to fifty rows. "Assigned to
 * me" alone is a screen and a half, so in practice the page WAS "assigned to
 * me": the other three existed, were fetched on every visit, and lived
 * somewhere between two and eight screens below the fold. Tabs put all four
 * counts on one line, which is the thing a personal work surface is actually
 * being asked.
 *
 * They are counted in the tab rather than in a heading inside the panel,
 * because the count is what makes the OTHER tabs worth clicking -- a tab that
 * only tells you what it holds after you open it is a worse index than a list
 * of headings.
 */
function WorkTab({ label, query }: { label: string; query: QueryLike<unknown[]> }) {
  return (
    <>
      <span>{label}</span>
      {/* No count until there is one. A "(0)" while the query is in flight is
          an answer we do not have yet, and the same wrong answer a signed-out
          visitor used to get. `data` is undefined until the first load lands,
          which is the query spelling of that distinction. */}
      {query.data ? (
        <Badge intent="neutral" variant="soft" size="sm">
          {query.data.length}
        </Badge>
      ) : null}
    </>
  );
}

function WorkPanel({
  query,
  loadingLabel,
  emptyLabel,
}: {
  query: QueryLike<Issue[]>;
  loadingLabel: string;
  emptyLabel: string;
}) {
  return (
    <AsyncSection
      query={query}
      loadingLabel={loadingLabel}
      isEmpty={(issues) => issues.length === 0}
      emptyTitle={emptyLabel}
      emptyTone="inline"
    >
      {(issues) => <IssueResultsTable issues={issues} columns={COLUMNS} />}
    </AsyncSection>
  );
}

function FlagRequestList({ entries, emptyLabel }: { entries: FlagRequestEntry[]; emptyLabel: string }) {
  // The two-hop resolution (a flag names an issue OR an attachment) lives in
  // queries.ts with every other read, keyed by the flag ids. Resolving it here
  // would put a fetch outside the cache, where no invalidation can reach it.
  const rowsQ = useFlagRequestIssues(entries);

  return (
    <AsyncSection
      query={rowsQ}
      loadingLabel="Loading flag requests..."
      isEmpty={(rows) => rows.length === 0}
      emptyTitle={emptyLabel}
      emptyTone="inline"
    >
      {(rows) => (
        <ul className="m-0 flex list-none flex-col gap-1 p-0">
          {rows.map(({ entry, issue }) => (
            <li key={entry.flag.id} className="space-x-1">
              <Badge intent="neutral" variant="outline" size="sm">
                {entry.flagType?.name ?? entry.flag.flagTypeId} {entry.flag.status}
              </Badge>
              {issue ? (
                <Link to={`/issues/${issue.id}`}>{issue.summary}</Link>
              ) : (
                <Muted>on an attachment</Muted>
              )}
            </li>
          ))}
        </ul>
      )}
    </AsyncSection>
  );
}

/**
 * The dashboard is all-or-nothing, so it is the page `RequireSession` was
 * written for.
 *
 * Every query behind it -- `notifications.list`, `cc.listMine`,
 * `votes.listMine`, `watchers.list`, `flags.listRequests` -- is `auth: "user"`,
 * so without an identity there is no half of this page worth rendering. The
 * `pending` slot is deliberately the header alone: showing either the page or
 * the sign-in wall while `users.me` is still in flight is a claim about the
 * visitor that has not been answered yet.
 */
export function DashboardPage() {
  return (
    <RequireSession
      pending={
        <Page className="dashboard-page">
          <DashboardHeader />
        </Page>
      }
      fallback={
        <Page className="dashboard-page">
          <DashboardHeader />
          <SignInRequired />
        </Page>
      }
    >
      <DashboardBody />
    </RequireSession>
  );
}

function DashboardHeader() {
  return (
    <PageHeader>
      <PageHeader.Title>My dashboard</PageHeader.Title>
    </PageHeader>
  );
}

function DashboardBody() {
  const session = useSession();
  // Inside RequireSession, so the server has answered and this is who. Read
  // from the shared session rather than a sixth `currentUser({})` call.
  const me = isSignedIn(session) ? session.user : null;
  const meId = me?.id ?? null;

  // Gated on the identity rather than resolving to an empty list without one:
  // "we do not know who you are yet" and "you have nothing assigned" are
  // different answers, and only the gate keeps the tab from claiming the
  // second. Inside RequireSession `meId` is always present, so the gate is a
  // statement about the query, not a branch anyone reaches.
  const assignedQ = useIssueSearch(
    { assigneeId: meId ?? undefined, limit: 50 },
    { enabled: Boolean(meId) },
  );
  const reportedQ = useIssueSearch(
    { reporterId: meId ?? undefined, limit: 50 },
    { enabled: Boolean(meId) },
  );
  const flagRequestsQ = useFlagRequests();
  const ccQ = useMyCc();
  const votesQ = useMyVotes();

  // Voting had a panel on every issue and nowhere to see what you had voted
  // for, so the budget it enforces -- votesPerUser, per product -- was
  // spendable and unauditable. Projected to the issue rows the shared table
  // takes, so it is the same table as the other three tabs.
  //
  // The projection is a QueryLike rather than a second query: the votes are
  // already cached under one key, and re-keying a view of them would be a
  // second entry holding the same answer. Only `data` changes; every other
  // field is the query's own, so the panel keeps the real loading, error and
  // refetch behaviour.
  const votedIssues = useMemo<QueryLike<Issue[]>>(
    () => ({
      data: votesQ.data?.map((row) => row.issue),
      error: votesQ.error,
      isPending: votesQ.isPending,
      isError: votesQ.isError,
      isFetching: votesQ.isFetching,
      refetch: votesQ.refetch,
    }),
    [votesQ.data, votesQ.error, votesQ.isPending, votesQ.isError, votesQ.isFetching, votesQ.refetch],
  );

  const setByMe = useMemo(() => flagRequestsQ.data?.setByMe ?? [], [flagRequestsQ.data]);
  const requestedOfMe = useMemo(() => flagRequestsQ.data?.requestedOfMe ?? [], [flagRequestsQ.data]);

  // The signed-out arm is RequireSession's job now, above. It used to live here
  // as the same two-clause boolean five pages each wrote, which answers a
  // four-state question with two answers -- so a slow `users.me` read as signed
  // in and the page rendered "Assigned to me (0)", telling a visitor they have
  // no issues when the truth is that we did not know who they were yet.
  return (
    <Page className="dashboard-page">
      <PageHeader>
        <PageHeader.Title>My dashboard</PageHeader.Title>
        <PageHeader.Description>
          What happened while you were away, and everything the tracker has connected you to.
        </PageHeader.Description>
      </PageHeader>

      <NotificationsPanel />

      {me && !me.isProvisioned ? (
        <Hint>
          No app activity yet for this identity -- your profile is created the first time you
          file, comment, or otherwise write something.
        </Hint>
      ) : null}

      <DashboardSection className="my-work">
        <SectionHeading>My work</SectionHeading>
        {/* keepMounted is deliberately NOT set: the panels hold issue tables of
            up to fifty rows each, and mounting all four would put three
            invisible tables in the document for every visit. The data is
            already fetched here at the page level, so switching a tab is a
            re-render and not a request. */}
        <Tabs defaultValue="assigned" lazyMount>
          <Tabs.List>
            <Tabs.Tab value="assigned">
              <WorkTab label="Assigned to me" query={assignedQ} />
            </Tabs.Tab>
            <Tabs.Tab value="reported">
              <WorkTab label="Reported by me" query={reportedQ} />
            </Tabs.Tab>
            <Tabs.Tab value="cc">
              <WorkTab label="CC'd on" query={ccQ} />
            </Tabs.Tab>
            <Tabs.Tab value="voted">
              <WorkTab label="Voted for" query={votedIssues} />
            </Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="assigned">
            <WorkPanel
              query={assignedQ}
              loadingLabel="Loading assigned issues..."
              emptyLabel="Nothing is assigned to you."
            />
          </Tabs.Panel>
          <Tabs.Panel value="reported">
            <WorkPanel
              query={reportedQ}
              loadingLabel="Loading reported issues..."
              emptyLabel="You have not reported an issue yet."
            />
          </Tabs.Panel>
          <Tabs.Panel value="cc">
            <WorkPanel
              query={ccQ}
              loadingLabel="Loading CC'd issues..."
              emptyLabel="You are not on any CC list."
            />
          </Tabs.Panel>
          <Tabs.Panel value="voted">
            <WorkPanel
              query={votedIssues}
              loadingLabel="Loading votes..."
              emptyLabel="You have not voted for any issue."
            />
          </Tabs.Panel>
        </Tabs>
      </DashboardSection>

      {/* Three short lists across the page rather than three more full-width
          bands down it. None of them is ever more than a handful of lines, and
          stacked they were what pushed the tables above them out of reach. */}
      <Grid minColWidth="24rem" gap={4}>
        <DashboardSection>
          <SectionHeading>Requests waiting on me</SectionHeading>
          <FlagRequestList entries={requestedOfMe} emptyLabel="No open flag requests directed at you." />
        </DashboardSection>

        <DashboardSection>
          <SectionHeading>My open flag requests</SectionHeading>
          <FlagRequestList entries={setByMe} emptyLabel="You have not requested any flags." />
        </DashboardSection>

        <WatchingPanel />
      </Grid>
    </Page>
  );
}
