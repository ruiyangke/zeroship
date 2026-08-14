import { useMemo } from "react";
import { Link } from "react-router-dom";
import { Badge, Grid, PageHeader, Tabs } from "@zeroship/ui";
import {
  getAttachment,
  getIssue,
  listFlagRequests,
  listMyCc,
  listMyVotes,
  searchIssues,
} from "../api";
import { ALL_ISSUE_COLUMNS, IssueResultsTable } from "../components/IssueResultsTable";
import { NotificationsPanel } from "../components/NotificationsPanel";
import { WatchingPanel } from "../components/WatchingPanel";
import { AsyncSection, SignInRequired } from "../components/StateViews";
import { toPromise, useAsync, type AsyncState } from "../components/rpc";
import { RequireSession, isSignedIn, useSession } from "../components/session";
import type { Issue, IssueDetail, FlagRequestEntry } from "../components/types";

type DashboardIssue = IssueDetail["issue"];

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
function WorkTab({ label, state }: { label: string; state: AsyncState<unknown[]> }) {
  return (
    <>
      <span>{label}</span>
      {/* No count until there is one. A "(0)" while the query is in flight is
          an answer we do not have yet, and the same wrong answer a signed-out
          visitor used to get. */}
      {state.status === "ready" ? (
        <Badge intent="neutral" variant="soft" size="sm">
          {state.data.length}
        </Badge>
      ) : null}
    </>
  );
}

function WorkPanel({
  state,
  reload,
  loadingLabel,
  emptyLabel,
}: {
  state: AsyncState<Issue[]>;
  reload: () => void;
  loadingLabel: string;
  emptyLabel: string;
}) {
  return (
    <AsyncSection
      state={state}
      onRetry={reload}
      loadingLabel={loadingLabel}
      isEmpty={(issues) => issues.length === 0}
      emptyTitle={emptyLabel}
      emptyTone="inline"
    >
      {(issues) => <IssueResultsTable issues={issues} columns={COLUMNS} />}
    </AsyncSection>
  );
}

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

function FlagRequestList({ entries, emptyLabel }: { entries: FlagRequestEntry[]; emptyLabel: string }) {
  const { state } = useAsync(async () => {
    const withIssueId = await Promise.all(
      entries.map(async (entry) => ({ entry, issueId: await flagIssueId(entry) })),
    );
    const issueIds = [...new Set(withIssueId.map((x) => x.issueId).filter((id): id is string => id !== null))];
    const issues = await Promise.all(
      issueIds.map((id) => toPromise(getIssue({ id })).catch(() => null)),
    );
    const byId = new Map<string, DashboardIssue>();
    for (const detail of issues) {
      if (detail) byId.set(detail.issue.id, detail.issue);
    }
    return withIssueId.map(({ entry, issueId }) => ({
      entry,
      issue: issueId ? byId.get(issueId) ?? null : null,
    }));
  }, [entries]);

  return (
    <AsyncSection
      state={state}
      loadingLabel="Loading flag requests..."
      isEmpty={(rows) => rows.length === 0}
      emptyTitle={emptyLabel}
      emptyTone="inline"
    >
      {(rows) => (
        <ul className="flag-request-list">
          {rows.map(({ entry, issue }) => (
            <li key={entry.flag.id}>
              <span className="chip">
                {entry.flagType?.name ?? entry.flag.flagTypeId} {entry.flag.status}
              </span>
              {issue ? (
                <Link to={`/issues/${issue.id}`}>{issue.summary}</Link>
              ) : (
                <span className="dim">on an attachment</span>
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
        <div className="page dashboard-page">
          <DashboardHeader />
        </div>
      }
      fallback={
        <div className="page dashboard-page">
          <DashboardHeader />
          <SignInRequired />
        </div>
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

  const assignedQ = useAsync(
    () => (meId ? searchIssues({ assigneeId: meId, limit: 50 }) : Promise.resolve([])),
    [meId],
  );
  const reportedQ = useAsync(
    () => (meId ? searchIssues({ reporterId: meId, limit: 50 }) : Promise.resolve([])),
    [meId],
  );
  const flagRequestsQ = useAsync(() => listFlagRequests({}), []);
  const ccQ = useAsync(() => listMyCc({}), []);
  const votesQ = useAsync(() => listMyVotes({}), []);

  // Voting had a panel on every issue and nowhere to see what you had voted
  // for, so the budget it enforces -- votesPerUser, per product -- was
  // spendable and unauditable. Projected to the issue rows the shared table
  // takes, so it is the same table as the other three tabs.
  const votedIssues = useMemo<AsyncState<Issue[]>>(
    () =>
      votesQ.state.status === "ready"
        ? { ...votesQ.state, data: votesQ.state.data.map((row) => row.issue) }
        : votesQ.state,
    [votesQ.state],
  );

  const setByMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.setByMe : []),
    [flagRequestsQ.state],
  );
  const requestedOfMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.requestedOfMe : []),
    [flagRequestsQ.state],
  );

  // The signed-out arm is RequireSession's job now, above. It used to live here
  // as the same two-clause boolean five pages each wrote, which answers a
  // four-state question with two answers -- so a slow `users.me` read as signed
  // in and the page rendered "Assigned to me (0)", telling a visitor they have
  // no issues when the truth is that we did not know who they were yet.
  return (
    <div className="page dashboard-page">
      <PageHeader>
        <PageHeader.Title>My dashboard</PageHeader.Title>
        <PageHeader.Description>
          What happened while you were away, and everything the tracker has connected you to.
        </PageHeader.Description>
      </PageHeader>

      <NotificationsPanel />

      {me && !me.isProvisioned ? (
        <p className="state-hint">
          No app activity yet for this identity -- your profile is created the first time you
          file, comment, or otherwise write something.
        </p>
      ) : null}

      <section className="dashboard-section my-work">
        <h2>My work</h2>
        {/* keepMounted is deliberately NOT set: the panels hold issue tables of
            up to fifty rows each, and mounting all four would put three
            invisible tables in the document for every visit. The data is
            already fetched here at the page level, so switching a tab is a
            re-render and not a request. */}
        <Tabs defaultValue="assigned" lazyMount>
          <Tabs.List>
            <Tabs.Tab value="assigned">
              <WorkTab label="Assigned to me" state={assignedQ.state} />
            </Tabs.Tab>
            <Tabs.Tab value="reported">
              <WorkTab label="Reported by me" state={reportedQ.state} />
            </Tabs.Tab>
            <Tabs.Tab value="cc">
              <WorkTab label="CC'd on" state={ccQ.state} />
            </Tabs.Tab>
            <Tabs.Tab value="voted">
              <WorkTab label="Voted for" state={votedIssues} />
            </Tabs.Tab>
            <Tabs.Indicator />
          </Tabs.List>
          <Tabs.Panel value="assigned">
            <WorkPanel
              state={assignedQ.state}
              reload={assignedQ.reload}
              loadingLabel="Loading assigned issues..."
              emptyLabel="Nothing is assigned to you."
            />
          </Tabs.Panel>
          <Tabs.Panel value="reported">
            <WorkPanel
              state={reportedQ.state}
              reload={reportedQ.reload}
              loadingLabel="Loading reported issues..."
              emptyLabel="You have not reported an issue yet."
            />
          </Tabs.Panel>
          <Tabs.Panel value="cc">
            <WorkPanel
              state={ccQ.state}
              reload={ccQ.reload}
              loadingLabel="Loading CC'd issues..."
              emptyLabel="You are not on any CC list."
            />
          </Tabs.Panel>
          <Tabs.Panel value="voted">
            <WorkPanel
              state={votedIssues}
              reload={votesQ.reload}
              loadingLabel="Loading votes..."
              emptyLabel="You have not voted for any issue."
            />
          </Tabs.Panel>
        </Tabs>
      </section>

      {/* Three short lists across the page rather than three more full-width
          bands down it. None of them is ever more than a handful of lines, and
          stacked they were what pushed the tables above them out of reach. */}
      <Grid minColWidth="24rem" gap={4} className="dashboard-asides">
        <section className="dashboard-section">
          <h2>Requests waiting on me</h2>
          <FlagRequestList entries={requestedOfMe} emptyLabel="No open flag requests directed at you." />
        </section>

        <section className="dashboard-section">
          <h2>My open flag requests</h2>
          <FlagRequestList entries={setByMe} emptyLabel="You have not requested any flags." />
        </section>

        <WatchingPanel />
      </Grid>
    </div>
  );
}
