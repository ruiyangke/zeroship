import { listNotifications, markNotificationRead } from "../api";
import { Button } from "@zeroship/ui";
import { AsyncSection } from "./StateViews";
import { useAsync } from "./rpc";

/**
 * The notification inbox.
 *
 * The nav has carried an unread badge since the UI was written, linking to the
 * dashboard -- which had nowhere to read the notifications it was counting. It
 * showed a number and offered no way to act on it. (For most of that time the
 * count was also always zero, because nothing wrote notifications at all.)
 */
export function NotificationsPanel() {
  const { state, reload } = useAsync(() => listNotifications({ limit: 50 }), []);

  const markRead = async (id: string) => {
    await markNotificationRead({ id });
    // Reloads rather than mutating in place: markRead also rewrites the KV
    // unread cache, and the nav badge reads that on its own next render. Two
    // components deriving the same count from different sources is how they
    // drift apart.
    reload();
  };

  return (
    <section className="dashboard-section notifications-panel">
      <h2>Notifications</h2>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading notifications..."
        isEmpty={(rows) => rows.length === 0}
        emptyTitle="Nothing new."
      >
        {(rows) => (
          <ul className="notification-list">
            {rows.map((row) => (
              <li key={row.id} className={row.isRead ? "read" : "unread"}>
                {row.bugId ? (
                  <a href={`#/bugs/${row.bugId}`}>{row.title}</a>
                ) : (
                  <span>{row.title}</span>
                )}
                {row.body ? <p className="notification-body">{row.body}</p> : null}
                {row.isRead ? null : (
                  <Button variant="gray" size="small"
                    onClick={() => void markRead(row.id)}
                  >
                    Mark read
                  </Button>
                )}
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
    </section>
  );
}
