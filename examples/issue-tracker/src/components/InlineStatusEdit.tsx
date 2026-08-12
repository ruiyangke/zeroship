// Inline status editor for the bug list. Reuses the server's own
// STATUS_TRANSITIONS table so the offered targets can never drift from what
// bugs.changeStatus actually accepts. Resolving is never a single dropdown
// pick: choosing RESOLVED opens a second, required resolution control, and
// only THEN does the mutation fire -- status and resolution stay two
// separate decisions, matching the detail page.
import { useState } from "react";
import { changeBugStatus, resolveBug } from "../api";
import {
  BUG_RESOLUTIONS,
  STATUS_TRANSITIONS,
  isBugStatus,
  type BugResolution,
  type BugStatus,
} from "../lib/workflow";
import { errorMessage } from "./rpc";
import type { Bug } from "./types";

type NonDuplicateResolution = Exclude<BugResolution, "DUPLICATE">;
const NON_DUPLICATE_RESOLUTIONS = BUG_RESOLUTIONS.filter(
  (r): r is NonDuplicateResolution => r !== "DUPLICATE",
);

export function InlineStatusEdit({
  bug,
  onChanged,
}: {
  bug: Bug;
  onChanged?: (bug: Bug) => void;
}) {
  const [pendingResolve, setPendingResolve] = useState(false);
  const [resolution, setResolution] = useState<NonDuplicateResolution>("FIXED");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const currentStatus: BugStatus | null = isBugStatus(bug.status) ? bug.status : null;
  const targets = currentStatus ? STATUS_TRANSITIONS[currentStatus] : [];
  if (targets.length === 0) {
    return <span className="status-readonly">{bug.status} (terminal)</span>;
  }

  const apply = async (nextStatus: string) => {
    setError(null);
    if (nextStatus === bug.status || !isBugStatus(nextStatus)) return;
    if (nextStatus === "RESOLVED") {
      setPendingResolve(true);
      return;
    }
    setBusy(true);
    try {
      const updated = await changeBugStatus({
        id: bug.id,
        status: nextStatus,
      });
      onChanged?.(updated);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const confirmResolve = async () => {
    setBusy(true);
    setError(null);
    try {
      const updated = await resolveBug({ id: bug.id, resolution });
      onChanged?.(updated);
      setPendingResolve(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="inline-status">
      <select
        value={bug.status}
        disabled={busy}
        onChange={(e) => void apply(e.target.value)}
      >
        <option value={bug.status}>{bug.status}</option>
        {targets.map((target) => (
          <option key={target} value={target}>
            {target}
          </option>
        ))}
      </select>
      {pendingResolve ? (
        <span className="inline-resolve">
          <select
            value={resolution}
            onChange={(e) => setResolution(e.target.value as NonDuplicateResolution)}
          >
            {NON_DUPLICATE_RESOLUTIONS.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
          <button type="button" className="btn ghost small" disabled={busy} onClick={() => void confirmResolve()}>
            Resolve
          </button>
          <button
            type="button"
            className="btn ghost small"
            disabled={busy}
            onClick={() => setPendingResolve(false)}
          >
            Cancel
          </button>
        </span>
      ) : null}
      {error ? <span className="field-error">{error}</span> : null}
    </div>
  );
}
