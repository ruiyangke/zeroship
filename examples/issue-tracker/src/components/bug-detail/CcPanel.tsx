import { useState } from "react";
import { addCc, listCc, removeCc } from "../../api";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import { UserPicker } from "../UserPicker";

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

  return (
    <section className="cc-panel">
      <h3>CC</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading CC list..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="Nobody is CC'd."
      >
        {(rows) => (
          <ul className="cc-list">
            {rows.map((row) => (
              <li key={row.id}>
                <span>{row.user?.name ?? row.userId}</span>
                <button type="button" className="btn ghost small" disabled={busy} onClick={() => void remove(row.userId)}>
                  Remove
                </button>
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
      {state.status !== "error" ? (
        <>
          <button type="button" className="btn ghost small" onClick={() => setPicking((v) => !v)}>
            Add CC
          </button>
          {picking ? <UserPicker onPick={(u) => void add(u.id)} /> : null}
        </>
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}
