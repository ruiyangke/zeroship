// The Bugzilla-fidelity centerpiece: status and resolution are always two
// separate controls, and moving to RESOLVED always requires an explicit,
// separate resolution pick before the mutation fires. Reopening and marking
// a duplicate are their own dedicated actions, not options folded into the
// status dropdown.
import { useState } from "react";
import { Select } from "@zeroship/ui";
import { changeBugStatus, markBugDuplicate, reopenBug, resolveBug } from "../../api";
import {
  BUG_RESOLUTIONS,
  STATUS_TRANSITIONS,
  isBugStatus,
  isOpenBugStatus,
  type BugResolution,
  type BugStatus,
} from "../../lib/workflow";
import { ResolutionBadge } from "../Badges";
import { errorMessage } from "../rpc";
import type { BugDetail } from "../types";

type NonDuplicateResolution = Exclude<BugResolution, "DUPLICATE">;
const NON_DUPLICATE_RESOLUTIONS = BUG_RESOLUTIONS.filter(
  (r): r is NonDuplicateResolution => r !== "DUPLICATE",
);

export function StatusControl({
  bug,
  onUpdated,
}: {
  bug: BugDetail["bug"];
  onUpdated: (bug: BugDetail["bug"]) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [resolution, setResolution] = useState<NonDuplicateResolution>("FIXED");
  const [showResolve, setShowResolve] = useState(false);
  const [duplicateOf, setDuplicateOf] = useState("");
  const [showDuplicate, setShowDuplicate] = useState(false);

  const currentStatus: BugStatus | null = isBugStatus(bug.status) ? bug.status : null;
  const targets = currentStatus ? STATUS_TRANSITIONS[currentStatus] : [];
  const openTargets = targets.filter((t) => t !== "RESOLVED");

  const run = async (action: () => BugDetail["bug"] | Promise<BugDetail["bug"]>) => {
    setBusy(true);
    setError(null);
    try {
      onUpdated(await action());
      setShowResolve(false);
      setShowDuplicate(false);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="status-control">
      <div className="status-control-row">
        <label>
          Status
          <Select
            value={bug.status}
            disabled={busy || targets.length === 0}
            aria-label="Status"
            onValueChange={(next) => {
              if (!next || next === bug.status || !isBugStatus(next)) return;
              void run(() => changeBugStatus({ id: bug.id, status: next }));
            }}
          >
            {/* The current status is listed first so the control can show it,
                then the states it can legally move to. The workflow decides
                that set -- this is not every status. */}
            <Select.Item value={bug.status}>{bug.status}</Select.Item>
            {openTargets.map((t) => (
              <Select.Item key={t} value={t}>
                {t}
              </Select.Item>
            ))}
          </Select>
        </label>
        {/* A VALUE, not a disabled input. Resolution is never typed here --
            it is chosen in the Resolve flow below, which also enforces the
            pairing with status. A greyed-out text box holding "--" says "you
            could edit this, but not right now", which is the opposite of
            true. */}
        <div className="status-resolution">
          <span className="field-label">Resolution</span>
          {bug.resolution ? (
            <ResolutionBadge resolution={bug.resolution} />
          ) : (
            <span className="dim">Unresolved</span>
          )}
        </div>
      </div>

      <div className="status-actions">
        {targets.includes("RESOLVED") ? (
          <button type="button" className="btn ghost small" disabled={busy} onClick={() => setShowResolve((v) => !v)}>
            Resolve...
          </button>
        ) : null}
        {currentStatus && !isOpenBugStatus(currentStatus) ? (
          <button type="button" className="btn ghost small" disabled={busy} onClick={() => void run(() => reopenBug({ id: bug.id }))}>
            Reopen
          </button>
        ) : null}
        <button type="button" className="btn ghost small" disabled={busy} onClick={() => setShowDuplicate((v) => !v)}>
          Mark as duplicate...
        </button>
      </div>

      {showResolve ? (
        <div className="inline-form">
          <label>
            Resolution (required to resolve)
            <Select
              value={resolution}
              onValueChange={(next) => setResolution(next as NonDuplicateResolution)}
              aria-label="Resolution (required to resolve)"
            >
              {NON_DUPLICATE_RESOLUTIONS.map((r) => (
                <Select.Item key={r} value={r}>
                  {r}
                </Select.Item>
              ))}
            </Select>
          </label>
          <button
            type="button"
            className="btn primary small"
            disabled={busy}
            onClick={() => void run(() => resolveBug({ id: bug.id, resolution }))}
          >
            Confirm resolve
          </button>
        </div>
      ) : null}

      {showDuplicate ? (
        <div className="inline-form">
          <label>
            Duplicate of (bug id)
            <input
              value={duplicateOf}
              onChange={(e) => setDuplicateOf(e.target.value)}
              placeholder="bug_..."
            />
          </label>
          <button
            type="button"
            className="btn primary small"
            disabled={busy || !duplicateOf.trim()}
            onClick={() => void run(() => markBugDuplicate({ id: bug.id, duplicateOfId: duplicateOf.trim() }))}
          >
            Confirm duplicate
          </button>
        </div>
      ) : null}

      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}
