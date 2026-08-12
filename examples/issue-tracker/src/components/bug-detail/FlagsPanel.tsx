// Flags are READ from the server via flags.list, not reconstructed.
//
// This panel used to derive each flag's current value by replaying the bug's
// activity log. That was exact only for non-multiplicable flag types -- several
// live flags of one type collapse to whichever was written last -- and it left
// "Clear" unusable, because clearing needs a flag id and the id was only ever
// returned by flags.set. A flag set in an earlier session could never be
// cleared at all. flags.list now returns the real rows, ids included, so both
// problems are gone and the caveat that used to sit at the bottom of this panel
// is deleted rather than reworded.
import { useCallback, useEffect, useState } from "react";
import { Select } from "@zeroship/ui";

import { clearFlag, listFlags, setFlag } from "../../api";
import { errorMessage } from "../rpc";
import type { FlagType } from "../types";
import { UserPicker } from "../UserPicker";

type FlagStatus = "+" | "-" | "?";
const FLAG_STATUSES: FlagStatus[] = ["+", "-", "?"];

type LiveFlag = Awaited<ReturnType<typeof listFlags>>["onBug"][number];

function FlagRow({
  bugId,
  flagType,
  live,
  onChanged,
}: {
  bugId: string;
  flagType: FlagType;
  live: readonly LiveFlag[];
  onChanged: () => void;
}) {
  const [status, setStatus] = useState<FlagStatus>("+");
  const [requesteeId, setRequesteeId] = useState<string | null>(null);
  const [pickingRequestee, setPickingRequestee] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Every live flag of this type, not just the last one. A multiplicable type
  // legitimately has several at once, and collapsing them was the old bug.
  const mine = live.filter((entry) => entry.flag.flagTypeId === flagType.id);

  const apply = async () => {
    setBusy(true);
    setError(null);
    try {
      await setFlag({
        flagTypeId: flagType.id,
        bugId,
        status,
        requesteeId: requesteeId ?? undefined,
      });
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const clear = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      await clearFlag({ id });
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
      {mine.length === 0 ? (
        <span className="badge flag-current-unset">not set</span>
      ) : (
        mine.map((entry) => (
          <span key={entry.flag.id} className="flag-current">
            <span className={`badge flag-current-${entry.flag.status === "+" ? "plus" : entry.flag.status === "-" ? "minus" : "question"}`}>
              {entry.flag.status}
            </span>
            {entry.requestee ? <span className="dim"> to {entry.requestee.name}</span> : null}
            {/* Clearing works for ANY live flag now, not only one this panel
                set, because the id comes from the server rather than from a
                setFlag response held in component state. */}
            <button
              type="button"
              className="btn ghost small"
              disabled={busy}
              onClick={() => void clear(entry.flag.id)}
            >
              Clear
            </button>
          </span>
        ))
      )}
      <Select
        value={status}
        onValueChange={(next) => setStatus(next as FlagStatus)}
        aria-label={`${flagType.name} status`}
      >
        {FLAG_STATUSES.filter((s) => s !== "?" || flagType.isRequestable).map((s) => (
          <Select.Item key={s} value={s}>
            {s}
          </Select.Item>
        ))}
      </Select>
      {status === "?" ? (
        <span className="flag-requestee">
          {requesteeId ? (
            <span className="dim">requestee: {requesteeId}</span>
          ) : (
            <button
              type="button"
              className="btn ghost small"
              onClick={() => setPickingRequestee((v) => !v)}
            >
              set requestee
            </button>
          )}
        </span>
      ) : null}
      <button type="button" className="btn ghost small" disabled={busy} onClick={() => void apply()}>
        Set
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
  onChanged,
}: {
  bugId: string;
  flagTypes: readonly FlagType[] | null;
  onChanged: () => void;
}) {
  const [live, setLive] = useState<readonly LiveFlag[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const result = await listFlags({ bugId });
      setLive(result.onBug);
      setLoadError(null);
    } catch (err) {
      // Surfaced rather than swallowed: an unreadable flag list rendering as
      // "not set" would claim, wrongly, that the bug carries no flags.
      setLoadError(errorMessage(err));
    }
  }, [bugId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const refresh = () => {
    void reload();
    onChanged();
  };

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
            <FlagRow
              key={flagType.id}
              bugId={bugId}
              flagType={flagType}
              live={live}
              onChanged={refresh}
            />
          ))}
        </ul>
      )}
      {loadError ? <p className="field-error">Could not load flags: {loadError}</p> : null}
    </section>
  );
}
