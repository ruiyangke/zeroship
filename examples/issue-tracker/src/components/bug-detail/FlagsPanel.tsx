// There is no flags.list(bugId) RPC (see SPEC's RPC surface: only
// flags.set / flags.clear / flags.listRequests). Current values shown here
// are reconstructed from the activity log (exact for non-multiplicable flag
// types, approximate for multiplicable ones -- see ../activity.ts). Because
// the server never hands back a flag id except at the moment flags.set
// resolves, "Clear" is only wired up for flags this panel itself just set --
// there is no way to look up the id of a flag set in an earlier session.
import { useState } from "react";
import { clearFlag, setFlag } from "../../api";
import { deriveLastValue } from "../activity";
import { errorMessage } from "../rpc";
import type { Activity, FlagType } from "../types";
import { UserPicker } from "../UserPicker";

type FlagStatus = "+" | "-" | "?";
const FLAG_STATUSES: FlagStatus[] = ["+", "-", "?"];

function FlagRow({
  bugId,
  flagType,
  activities,
  onChanged,
}: {
  bugId: string;
  flagType: FlagType;
  activities: readonly Activity[];
  onChanged: () => void;
}) {
  const current = deriveLastValue(activities, `flag.${flagType.name}`);
  const [status, setStatus] = useState<FlagStatus>("+");
  const [requesteeId, setRequesteeId] = useState<string | null>(null);
  const [pickingRequestee, setPickingRequestee] = useState(false);
  const [knownFlagId, setKnownFlagId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const apply = async () => {
    setBusy(true);
    setError(null);
    try {
      const flag = await setFlag({
        flagTypeId: flagType.id,
        bugId,
        status,
        requesteeId: requesteeId ?? undefined,
      });
      setKnownFlagId(flag.id);
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const clear = async () => {
    if (!knownFlagId) return;
    setBusy(true);
    setError(null);
    try {
      await clearFlag({ id: knownFlagId });
      setKnownFlagId(null);
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <li className="flag-row">
      <span className="flag-name">{flagType.name}</span>
      <span className={`badge flag-current-${(current ?? "unset").toLowerCase().replace(/[^a-z]/g, "x")}`}>
        {current ?? "not set"}
      </span>
      <select value={status} onChange={(e) => setStatus(e.target.value as FlagStatus)}>
        {FLAG_STATUSES.filter((s) => s !== "?" || flagType.isRequestable).map((s) => (
          <option key={s} value={s}>
            {s}
          </option>
        ))}
      </select>
      {status === "?" ? (
        <span className="flag-requestee">
          {requesteeId ? (
            <span className="dim">requestee: {requesteeId}</span>
          ) : (
            <button type="button" className="btn ghost small" onClick={() => setPickingRequestee((v) => !v)}>
              set requestee
            </button>
          )}
        </span>
      ) : null}
      <button type="button" className="btn ghost small" disabled={busy} onClick={() => void apply()}>
        Set
      </button>
      <button type="button" className="btn ghost small" disabled={busy || !knownFlagId} onClick={() => void clear()}>
        Clear
      </button>
      {pickingRequestee ? (
        <UserPicker
          onPick={(u) => {
            setRequesteeId(u.id);
            setPickingRequestee(false);
          }}
        />
      ) : null}
      {error ? <p className="field-error">{error}</p> : null}
    </li>
  );
}

export function FlagsPanel({
  bugId,
  flagTypes,
  activities,
  onChanged,
}: {
  bugId: string;
  flagTypes: readonly FlagType[] | null;
  activities: readonly Activity[];
  onChanged: () => void;
}) {
  const bugFlagTypes = flagTypes?.filter((t) => t.targetType === "bug") ?? [];
  return (
    <section className="flags-panel">
      <h3>Flags</h3>
      {flagTypes === null ? (
        <p className="state-hint small">Sign in to see and set flags for this product.</p>
      ) : bugFlagTypes.length === 0 ? (
        <p className="state-hint small">This product defines no bug-level flag types.</p>
      ) : (
        <ul className="flag-list">
          {bugFlagTypes.map((flagType) => (
            <FlagRow key={flagType.id} bugId={bugId} flagType={flagType} activities={activities} onChanged={onChanged} />
          ))}
        </ul>
      )}
      <p className="state-hint small">
        Values above are reconstructed from activity history (there is no direct flags list
        for a bug). Clearing only works for a flag this panel set in the current session.
      </p>
    </section>
  );
}
