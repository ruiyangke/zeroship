import { useState } from "react";
import { Button } from "@zeroship/ui";
import { addCc, removeCc } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage } from "../rpc";
import { useAppMutation, useCc } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
import { UserPicker } from "../UserPicker";
import { Absent, Pending } from "./Absent";
import { RailDisclosure } from "./RailDisclosure";

export function CcPanel({ issueId }: { issueId: string }) {
  const ccQ = useCc(issueId);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const addUser = useAppMutation(
    (userId: string) => addCc({ issueId, userId }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const removeUser = useAppMutation(
    (userId: string) => removeCc({ issueId, userId }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const busy = addUser.isPending || removeUser.isPending;

  const add = async (userId: string) => {
    setError(null);
    try {
      await addUser.mutateAsync(userId);
      // Closing the picker is a UI decision this component still owns. Only
      // the refresh moved out.
      setPicking(false);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const remove = async (userId: string) => {
    setError(null);
    try {
      await removeUser.mutateAsync(userId);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  // The resting line. Names while there are few, a count once the list is
  // longer than the rail is wide -- three names is already 30 characters.
  // No data means no claim: a Skeleton while the query is pending, never `--`.
  const cc = ccQ.data;
  const summary = !cc ? (
    <Pending width="5rem" />
  ) : cc.length === 0 ? (
    <Absent />
  ) : cc.length <= 2 ? (
    <>{cc.map((row) => row.user?.name ?? row.userId).join(", ")}</>
  ) : (
    <>{cc.length} people</>
  );

  return (
    <RailDisclosure label="CC" summary={summary} action="Add">
    <section className="cc-panel">
      <AsyncSection
        query={ccQ}
        loadingLabel="Loading CC list..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="Nobody is CC'd."
        emptyTone="inline"
      >
        {(rows) => (
          <ul className="cc-list">
            {rows.map((row) => (
              <li key={row.id}>
                <span>{row.user?.name ?? row.userId}</span>
                <Button variant="gray" size="small" disabled={busy} onClick={() => void remove(row.userId)}>
                  Remove
                </Button>
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
      {!ccQ.isError ? (
        <>
          <Button variant="gray" size="small" onClick={() => setPicking((v) => !v)}>
            Add CC
          </Button>
          {picking ? <UserPicker onPick={(u) => void add(u.id)} /> : null}
        </>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </section>
    </RailDisclosure>
  );
}
