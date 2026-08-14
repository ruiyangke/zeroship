import { useMemo } from "react";
import { Link } from "react-router-dom";
import { PageHeader } from "@zeroship/ui";
import {
  currentUser,
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
import { AsyncSection, ErrorState } from "../components/StateViews";
import { isUnauthenticated, toPromise, useAsync } from "../components/rpc";
import type { Issue, IssueDetail, FlagRequestEntry } from "../components/types";

type DashboardIssue = IssueDetail["issue"];

const COLUMNS = ALL_ISSUE_COLUMNS.map((c) => c.key).filter((c) => c !== "reporter");

function IssueSection({ title, issues }: { title: string; issues: Issue[] }) {
  return (
    <section className="dashboard-section">
      <h2>
        {title} <span className="dim">({issues.length})</span>
      </h2>
      {issues.length === 0 ? (
        <p className="state-hint small">Nothing here.</p>
      ) : (
        <IssueResultsTable issues={issues} columns={COLUMNS} />
      )}
    </section>
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

export function DashboardPage() {
  const { state: userState } = useAsync(() => currentUser({}), []);
  const meId = userState.status === "ready" ? userState.data.id : null;

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

  const setByMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.setByMe : []),
    [flagRequestsQ.state],
  );
  const requestedOfMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.requestedOfMe : []),
    [flagRequestsQ.state],
  );

  // Signed out, the whole page is one answer: sign in. Without this the issue
  // sections fell back to Promise.resolve([]) and rendered "Assigned to me (0)
  // -- Nothing here", which tells a visitor they have no issues when the truth
  // is that we do not know who they are. searchIssues is anonymous and returns
  // an empty list rather than a 401, so nothing downstream could tell the two
  // apart. StateViews says this in its own header: a 401 is a sign-in prompt,
  // never an empty list.
  if (userState.status === "error" && isUnauthenticated(userState.error)) {
    return (
      <div className="page dashboard-page">
        <PageHeader>
          <PageHeader.Title>My dashboard</PageHeader.Title>
        </PageHeader>
        <ErrorState error={userState.error} />
      </div>
    );
  }

  return (
    <div className="page dashboard-page">
      <PageHeader>
        <PageHeader.Title>My dashboard</PageHeader.Title>
      </PageHeader>

      <NotificationsPanel />

      {userState.status === "ready" && !userState.data.isProvisioned ? (
        <p className="state-hint">
          No app activity yet for this identity -- your profile is created the first time you
          file, comment, or otherwise write something.
        </p>
      ) : null}

      <AsyncSection state={assignedQ.state} onRetry={assignedQ.reload} loadingLabel="Loading assigned issues...">
        {(issues) => <IssueSection title="Assigned to me" issues={issues} />}
      </AsyncSection>

      <AsyncSection state={reportedQ.state} onRetry={reportedQ.reload} loadingLabel="Loading reported issues...">
        {(issues) => <IssueSection title="Reported by me" issues={issues} />}
      </AsyncSection>

      <section className="dashboard-section">
        <h2>Requests waiting on me</h2>
        <FlagRequestList entries={requestedOfMe} emptyLabel="No open flag requests directed at you." />
      </section>

      <section className="dashboard-section">
        <h2>My open flag requests</h2>
        <FlagRequestList entries={setByMe} emptyLabel="You have not requested any flags." />
      </section>

      {/* Was a hardcoded "(0)" with a note saying no reverse index existed.
          The index did exist (issueCc.userId); what was missing was a procedure
          reading it, which cc.listMine now is. */}
      {/* Voting had a panel on every issue and nowhere to see what you had
          voted for, so the budget it enforces -- votesPerUser, per product --
          was spendable and unauditable. */}
      <AsyncSection
        state={votesQ.state}
        onRetry={votesQ.reload}
        loadingLabel="Loading votes..."
        isEmpty={(rows) => rows.length === 0}
        emptyTitle="You have not voted for any issue."
      >
        {(rows) => (
          <IssueSection
            title="Issues I voted for"
            issues={rows.map((row) => row.issue)}
          />
        )}
      </AsyncSection>

      <WatchingPanel />

      <AsyncSection state={ccQ.state} onRetry={ccQ.reload} loadingLabel="Loading CC'd issues...">
        {(issues) => <IssueSection title="Issues I'm CC'd on" issues={issues} />}
      </AsyncSection>
    </div>
  );
}
