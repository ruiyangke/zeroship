import { useState } from "react";
import { Button, Stack } from "@zeroship/ui";

import { addWatcher, removeWatcher } from "../api";
import { invalidatedBy } from "../lib/query-keys";
import { useAppMutation, useWatchers } from "../lib/queries";
import { errorMessage } from "./rpc";
import { UserPicker } from "./UserPicker";
import { DashboardSection } from "./DashboardSection";
import { MemberList, MemberListItem } from "./MemberList";
import { FieldError, Hint } from "./AppPrimitives";

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
  const watchingQ = useWatchers();
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const add = useAppMutation(
    (watchedId: string) => addWatcher({ watchedId }),
    () => invalidatedBy.watchersChanged(),
  );
  const remove = useAppMutation(
    (watchedId: string) => removeWatcher({ watchedId }),
    () => invalidatedBy.watchersChanged(),
  );
  const watching = watchingQ.data ?? [];
  const busy = add.isPending || remove.isPending;
  const shownError = error ?? (watchingQ.isError ? errorMessage(watchingQ.error) : null);

  const watch = async (watchedId: string) => {
    setError(null);
    try {
      await add.mutateAsync(watchedId);
      setPicking(false);
    } catch (err) {
      // "you cannot watch yourself" arrives here. It is a real answer, not a
      // failure, so it is shown rather than swallowed.
      setError(errorMessage(err));
    }
  };

  const unwatch = async (watchedId: string) => {
    setError(null);
    try {
      await remove.mutateAsync(watchedId);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <DashboardSection className="watching-panel">
      <h2>People I watch</h2>
      <Hint>
        You are notified about issues they report, are assigned, or are CC'd on.
      </Hint>

      {watching.length === 0 ? (
        <Hint>Not watching anyone.</Hint>
      ) : (
        <Stack gap={1}>
          <MemberList>
            {watching.map((row) => (
              <MemberListItem key={row.id}>
                <span>{row.watched?.name ?? row.watched?.handle ?? row.watchedId}</span>
                <Button
                  variant="gray"
                  size="sm"
                  intent="destructive"
                  disabled={busy}
                  onClick={() => void unwatch(row.watchedId)}
                >
                  Stop watching
                </Button>
              </MemberListItem>
            ))}
          </MemberList>
        </Stack>
      )}

      <Button variant="gray" size="sm" disabled={busy} onClick={() => setPicking((v) => !v)}>
        {picking ? "Cancel" : "Watch someone"}
      </Button>
      {picking ? <UserPicker onPick={(user) => void watch(user.id)} /> : null}
      {shownError ? <FieldError>{shownError}</FieldError> : null}
    </DashboardSection>
  );
}
