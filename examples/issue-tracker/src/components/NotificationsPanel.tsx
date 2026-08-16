import { markNotificationRead } from "../api";
import { Link } from "react-router-dom";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useNotifications } from "../lib/queries";
import { Button } from "../ui/Button";
import { ListView } from "../ui/ListView";
import { RichText } from "./RichText";
import { AsyncSection } from "./StateViews";
import { DashboardSection } from "./DashboardSection";
import { Hint, SectionHeading } from "./AppPrimitives";

/**
 * The notification inbox.
 *
 * The nav has carried an unread badge since the UI was written, linking to the
 * dashboard -- which had nowhere to read the notifications it was counting. It
 * showed a number and offered no way to act on it. (For most of that time the
 * count was also always zero, because nothing wrote notifications at all.)
 *
 * ONE row shape, which is the fix here. Every row used to lay itself out from
 * whatever it happened to contain: unread rows drew a tinted box and read ones
 * did not, "Mark read" sat inline after the title on a row with no body and
 * dropped to its own line under one with a body, and a row whose title was not
 * a link lost the only coloured thing on it. Four visual treatments for two
 * states, so the thing worth seeing at a glance -- read or unread -- was the
 * one thing you could not see at a glance.
 *
 * ListView gives every row the same lockup: a leading unread dot, the title and
 * body in the content column, and the action in the trailing slot. Read and
 * unread now differ by exactly two signals, the dot and the muting, and nothing
 * else moves.
 */
export function NotificationsPanel() {
  const notificationsQ = useNotifications(50);

  // Invalidates rather than mutating the row in place: markRead also rewrites
  // the KV unread cache that the nav badge counts, and two components deriving
  // the same count from different sources is how they drift apart.
  // `notificationsChanged` names the whole `notifications` prefix for exactly
  // that reason -- it drops this list AND the unread count together, so no
  // caller has to remember the badge exists.
  const markRead = useAppMutation(
    (id: string) => markNotificationRead({ id }),
    () => invalidatedBy.notificationsChanged(),
  );

  return (
    <DashboardSection className="notifications-panel">
      <SectionHeading>Notifications</SectionHeading>
      <AsyncSection
        query={notificationsQ}
        loadingLabel="Loading notifications..."
        isEmpty={(rows) => rows.length === 0}
        emptyTitle="Nothing new."
      >
        {(rows) => (
          <>
            {/* Say how many, and how many are still unread. The list is a
                bounded scroll container -- without a count you cannot tell a
                full inbox from a nearly empty one, and the fetch caps at 50
                anyway, which is worth admitting rather than implying the
                inbox is exactly this long. */}
            <Hint>
              {rows.filter((row) => !row.isRead).length} unread of {rows.length} shown
              {rows.length >= 50 ? " (most recent 50)" : ""}
            </Hint>
            <ListView
              className="max-h-88 overflow-y-auto"
              density="compact"
              items={rows.map((row) => ({
                id: row.id,
                // The dot is the whole unread signal, in a column of its own so
                // the titles below it stay aligned whether or not it is there.
                leading: (
                  <span
                    className={`mt-2 block size-2 rounded-full ${
                      row.isRead ? "bg-transparent" : "bg-accent-strong"
                    }`}
                    aria-hidden="true"
                  />
                ),
                title: (
                  <span className={row.isRead ? "text-ink-muted [&_a]:text-ink-secondary" : undefined}>
                    {row.issueId ? (
                      <Link to={`/issues/${row.issueId}`}>{row.title}</Link>
                    ) : (
                      row.title
                    )}
                  </span>
                ),
                // Rendered, not printed. Comment bodies are markdown now, so
                // this row was showing literal asterisks and backticks --
                // the inbox was the surface that never got checked when the
                // storage format changed. Through the same read-only
                // renderer as the thread, so the same schema decides what a
                // notification may contain.
                description: row.body ? (
                  <div className="m-0 line-clamp-2 text-base text-ink-secondary [&_.rich-text>*]:m-0 [&_.rich-text>*]:text-base">
                    <RichText markdown={row.body} />
                  </div>
                ) : undefined,
                // One place for the action, on every row that has one. It used
                // to sit inline after the title on a row with no body and drop
                // to a line of its own under a row with one, so the same
                // control appeared in two places down a single list.
                trailing: row.isRead ? undefined : (
                  <Button variant="gray" onClick={() => markRead.mutate(row.id)}>
                    Mark read
                  </Button>
                ),
              }))}
            />
          </>
        )}
      </AsyncSection>
    </DashboardSection>
  );
}
