import { useMemo } from "react";
import { currentUser, getAttachment, getBug, listFlagRequests, listMyCc, searchBugs } from "../api";
import { ALL_BUG_COLUMNS, BugResultsTable } from "../components/BugResultsTable";
import { NotificationsPanel } from "../components/NotificationsPanel";
import { AsyncSection } from "../components/StateViews";
import { toPromise, useAsync } from "../components/rpc";
import { useBugLookups } from "../components/useBugLookups";
import type { Bug, BugDetail, FlagRequestEntry } from "../components/types";

type DashboardBug = BugDetail["bug"];

const COLUMNS = ALL_BUG_COLUMNS.map((c) => c.key).filter((c) => c !== "reporter");

function BugSection({
  title,
  bugs,
  lookups,
}: {
  title: string;
  bugs: Bug[];
  lookups: ReturnType<typeof useBugLookups>;
}) {
  return (
    <section className="dashboard-section">
      <h2>
        {title} <span className="dim">({bugs.length})</span>
      </h2>
      {bugs.length === 0 ? (
        <p className="state-hint small">Nothing here.</p>
      ) : (
        <BugResultsTable bugs={bugs} columns={COLUMNS} {...lookups} />
      )}
    </section>
  );
}

async function flagBugId(entry: FlagRequestEntry): Promise<string | null> {
  if (entry.flag.bugId) return entry.flag.bugId;
  if (!entry.flag.attachmentId) return null;
  try {
    const { attachment } = await getAttachment({ id: entry.flag.attachmentId });
    return attachment.bugId;
  } catch {
    return null;
  }
}

function FlagRequestList({ entries, emptyLabel }: { entries: FlagRequestEntry[]; emptyLabel: string }) {
  const { state } = useAsync(async () => {
    const withBugId = await Promise.all(
      entries.map(async (entry) => ({ entry, bugId: await flagBugId(entry) })),
    );
    const bugIds = [...new Set(withBugId.map((x) => x.bugId).filter((id): id is string => id !== null))];
    const bugs = await Promise.all(
      bugIds.map((id) => toPromise(getBug({ id })).catch(() => null)),
    );
    const byId = new Map<string, DashboardBug>();
    for (const detail of bugs) {
      if (detail) byId.set(detail.bug.id, detail.bug);
    }
    return withBugId.map(({ entry, bugId }) => ({
      entry,
      bug: bugId ? byId.get(bugId) ?? null : null,
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
          {rows.map(({ entry, bug }) => (
            <li key={entry.flag.id}>
              <span className="chip">
                {entry.flagType?.name ?? entry.flag.flagTypeId} {entry.flag.status}
              </span>
              {bug ? (
                <a href={`#/bugs/${bug.id}`}>{bug.summary}</a>
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
  // Resolved once for the whole page. The three bug tables below would
  // otherwise each fetch products and users for themselves.
  const lookups = useBugLookups();
  const { state: userState } = useAsync(() => currentUser({}), []);
  const meId = userState.status === "ready" ? userState.data.id : null;

  const assignedQ = useAsync(
    () => (meId ? searchBugs({ assigneeId: meId, limit: 50 }) : Promise.resolve([])),
    [meId],
  );
  const reportedQ = useAsync(
    () => (meId ? searchBugs({ reporterId: meId, limit: 50 }) : Promise.resolve([])),
    [meId],
  );
  const flagRequestsQ = useAsync(() => listFlagRequests({}), []);
  const ccQ = useAsync(() => listMyCc({}), []);

  const setByMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.setByMe : []),
    [flagRequestsQ.state],
  );
  const requestedOfMe = useMemo(
    () => (flagRequestsQ.state.status === "ready" ? flagRequestsQ.state.data.requestedOfMe : []),
    [flagRequestsQ.state],
  );

  return (
    <div className="page dashboard-page">
      <h1>My dashboard</h1>

      <NotificationsPanel />

      {userState.status === "ready" && !userState.data.isProvisioned ? (
        <p className="state-hint">
          No app activity yet for this identity -- your profile is created the first time you
          file, comment, or otherwise write something.
        </p>
      ) : null}

      <AsyncSection state={assignedQ.state} onRetry={assignedQ.reload} loadingLabel="Loading assigned bugs...">
        {(bugs) => <BugSection title="Assigned to me" bugs={bugs} lookups={lookups} />}
      </AsyncSection>

      <AsyncSection state={reportedQ.state} onRetry={reportedQ.reload} loadingLabel="Loading reported bugs...">
        {(bugs) => <BugSection title="Reported by me" bugs={bugs} lookups={lookups} />}
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
          The index did exist (bugCc.userId); what was missing was a procedure
          reading it, which cc.listMine now is. */}
      <AsyncSection state={ccQ.state} onRetry={ccQ.reload} loadingLabel="Loading CC'd bugs...">
        {(bugs) => <BugSection title="Bugs I'm CC'd on" bugs={bugs} lookups={lookups} />}
      </AsyncSection>
    </div>
  );
}
