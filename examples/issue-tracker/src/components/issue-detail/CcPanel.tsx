import { useState } from "react";
import { Button } from "@zeroship/ui";
import { addCc, listCc, removeCc } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import { UserPicker } from "../UserPicker";
import { Absent, Pending } from "./Absent";
import { RailDisclosure } from "./RailDisclosure";

export function CcPanel({ issueId }: { issueId: string }) {
  const { state, reload } = useAsync(() => listCc({ issueId }), [issueId]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const add = async (userId: string) => {
    setBusy(true);
    setError(null);
    try {
      await addCc({ issueId, userId });
      setPicking(false);
      reload();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (userId: string) => {
    setBusy(true);
    setError(null);
    try {
      await removeCc({ issueId, userId });
      reload();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  // The resting line. Names while there are few, a count once the list is
  // longer than the rail is wide -- three names is already 30 characters.
  const summary =
    state.status !== "ready" ? (
      <Pending width="5rem" />
    ) : state.data.length === 0 ? (
      <Absent />
    ) : state.data.length <= 2 ? (
      <>{state.data.map((row) => row.user?.name ?? row.userId).join(", ")}</>
    ) : (
      <>{state.data.length} people</>
    );

  return (
    <RailDisclosure label="CC" summary={summary} action="Add">
    <section className="cc-panel">
      <AsyncSection
        state={state}
        onRetry={reload}
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
      {state.status !== "error" ? (
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
