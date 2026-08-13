import { useState } from "react";
import { Button } from "@zeroship/ui";
import { addCc, listCc, removeCc } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import { UserPicker } from "../UserPicker";
import { RailDisclosure } from "./RailDisclosure";

export function CcPanel({ bugId }: { bugId: string }) {
  const { state, reload } = useAsync(() => listCc({ bugId }), [bugId]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const add = async (userId: string) => {
    setBusy(true);
    setError(null);
    try {
      await addCc({ bugId, userId });
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
      await removeCc({ bugId, userId });
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
      <span className="dim">--</span>
    ) : state.data.length === 0 ? (
      <span className="dim">nobody</span>
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
