import { useCallback, useEffect, useState } from "react";
import { Button, Stack } from "@zeroship/ui";

import { addWatcher, listWatchers, removeWatcher } from "../api";
import { errorMessage } from "./rpc";
import { UserPicker } from "./UserPicker";

/**
 * Bugzilla's user watching: you also hear about issues the watched person is
 * involved in.
 *
 * The three procedures behind this existed with no UI at all, so the feature
 * was unreachable -- but unlike the flags surface it was not dead. The
 * notification fanout already reads `watchers` and includes them alongside the
 * assignee, reporter, QA contact and CC list, so the only missing piece was a
 * way to say who you watch.
 *
 * On the dashboard rather than a profile page: watching is about what lands in
 * your inbox, and the inbox is here.
 */
export function WatchingPanel() {
  const [watching, setWatching] = useState<Awaited<ReturnType<typeof listWatchers>>>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const load = useCallback(async () => {
    try {
      setWatching(await listWatchers({}));
      setError(null);
    } catch (err) {
      setError(errorMessage(err));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const watch = async (watchedId: string) => {
    setBusy(true);
    setError(null);
    try {
      await addWatcher({ watchedId });
      setPicking(false);
      await load();
    } catch (err) {
      // "you cannot watch yourself" arrives here. It is a real answer, not a
      // failure, so it is shown rather than swallowed.
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const unwatch = async (watchedId: string) => {
    setBusy(true);
    setError(null);
    try {
      await removeWatcher({ watchedId });
      await load();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="dashboard-section watching-panel">
      <h2>People I watch</h2>
      <p className="state-hint small">
        You are notified about issues they report, are assigned, or are CC'd on.
      </p>

      {watching.length === 0 ? (
        <p className="state-hint small">Not watching anyone.</p>
      ) : (
        <Stack gap={1}>
          <ul className="member-list">
            {watching.map((row) => (
              <li key={row.id}>
                <span>{row.watched?.name ?? row.watched?.handle ?? row.watchedId}</span>
                <Button
                  variant="gray"
                  size="small"
                  intent="destructive"
                  disabled={busy}
                  onClick={() => void unwatch(row.watchedId)}
                >
                  Stop watching
                </Button>
              </li>
            ))}
          </ul>
        </Stack>
      )}

      <Button variant="gray" size="small" disabled={busy} onClick={() => setPicking((v) => !v)}>
        {picking ? "Cancel" : "Watch someone"}
      </Button>
      {picking ? <UserPicker onPick={(user) => void watch(user.id)} /> : null}
      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}
