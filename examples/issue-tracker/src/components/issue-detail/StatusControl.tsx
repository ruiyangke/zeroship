// The Bugzilla-fidelity centerpiece: status and resolution are always two
// separate controls, and moving to RESOLVED always requires an explicit,
// separate resolution pick before the mutation fires. Reopening and marking
// a duplicate are their own dedicated actions, not options folded into the
// status dropdown.
import { useState } from "react";
import { Button, Field, Input, Select } from "@zeroship/ui";
import { changeIssueStatus, markIssueDuplicate, reopenIssue, resolveIssue } from "../../api";
import {
  ISSUE_RESOLUTIONS,
  STATUS_TRANSITIONS,
  isIssueStatus,
  isOpenIssueStatus,
  type IssueResolution,
  type IssueStatus,
} from "../../lib/workflow";
import { invalidatedBy } from "../../lib/query-keys";
import { useAppMutation } from "../../lib/queries";
import { ResolutionBadge } from "../Badges";
import { FieldError, InlineForm, Muted } from "../AppPrimitives";
import { errorMessage } from "../rpc";
import type { IssueDetail } from "../types";
import { RailSection } from "./RailSection";

type NonDuplicateResolution = Exclude<IssueResolution, "DUPLICATE">;
const NON_DUPLICATE_RESOLUTIONS = ISSUE_RESOLUTIONS.filter(
  (r): r is NonDuplicateResolution => r !== "DUPLICATE",
);

export function StatusControl({ issue }: { issue: IssueDetail["issue"] }) {
  const [error, setError] = useState<string | null>(null);
  const [resolution, setResolution] = useState<NonDuplicateResolution>("FIXED");
  const [showResolve, setShowResolve] = useState(false);
  const [duplicateOf, setDuplicateOf] = useState("");
  const [showDuplicate, setShowDuplicate] = useState(false);

  const currentStatus: IssueStatus | null = isIssueStatus(issue.status) ? issue.status : null;
  const targets = currentStatus ? STATUS_TRANSITIONS[currentStatus] : [];
  const openTargets = targets.filter((t) => t !== "RESOLVED");

  const changeStatus = useAppMutation(
    (status: IssueStatus) => changeIssueStatus({ id: issue.id, status }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const reopen = useAppMutation(
    () => reopenIssue({ id: issue.id }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const resolve = useAppMutation(
    (nextResolution: NonDuplicateResolution) =>
      resolveIssue({ id: issue.id, resolution: nextResolution }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const markDuplicate = useAppMutation(
    (duplicateOfId: string) => markIssueDuplicate({ id: issue.id, duplicateOfId }),
    () => invalidatedBy.issueChanged(issue.id),
  );
  const busy =
    changeStatus.isPending || reopen.isPending || resolve.isPending || markDuplicate.isPending;

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    try {
      await action();
      // Closing these forms is local UI state, so it stays here. The mutation
      // owns refreshing the issue and every report derived from it.
      setShowResolve(false);
      setShowDuplicate(false);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <div className="col-span-full mt-3 mb-1 border-0 bg-transparent p-0">
      <RailSection title="Status" />
      <div className="flex flex-wrap gap-2">
        <Field>
          <Select
            value={issue.status}
            disabled={busy || targets.length === 0}
            aria-label="Status"
            onValueChange={(next) => {
              if (!next || next === issue.status || !isIssueStatus(next)) return;
              void run(() => changeStatus.mutateAsync(next));
            }}
          >
            {/* The current status is listed first so the control can show it,
                then the states it can legally move to. The workflow decides
                that set -- this is not every status. */}
            <Select.Item value={issue.status}>{issue.status}</Select.Item>
            {openTargets.map((t) => (
              <Select.Item key={t} value={t}>
                {t}
              </Select.Item>
            ))}
          </Select>
        </Field>
        {/* A VALUE, not a disabled input. Resolution is never typed here --
            it is chosen in the Resolve flow below, which also enforces the
            pairing with status. A greyed-out text box holding "--" says "you
            could edit this, but not right now", which is the opposite of
            true. */}
        <div className="flex items-center gap-2">
          <span className="text-base font-medium text-ink-muted">Resolution</span>
          {issue.resolution ? (
            <ResolutionBadge resolution={issue.resolution} />
          ) : (
            <Muted>Unresolved</Muted>
          )}
        </div>
      </div>

      <div className="mt-2 flex flex-wrap gap-2">
        {targets.includes("RESOLVED") ? (
          <Button variant="gray" size="sm" disabled={busy} onClick={() => setShowResolve((v) => !v)}>
            Resolve...
          </Button>
        ) : null}
        {currentStatus && !isOpenIssueStatus(currentStatus) ? (
          <Button variant="gray" size="sm" disabled={busy} onClick={() => void run(() => reopen.mutateAsync(undefined))}>
            Reopen
          </Button>
        ) : null}
        <Button variant="gray" size="sm" disabled={busy} onClick={() => setShowDuplicate((v) => !v)}>
          Mark as duplicate...
        </Button>
      </div>

      {showResolve ? (
        <InlineForm>
          <Field>
            <Field.Label>Resolution (required to resolve)</Field.Label>
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
          </Field>
          <Button variant="filled" size="sm"
            disabled={busy}
            onClick={() => void run(() => resolve.mutateAsync(resolution))}
          >
            Confirm resolve
          </Button>
        </InlineForm>
      ) : null}

      {showDuplicate ? (
        <InlineForm>
          <Field>
            <Field.Label>Duplicate of</Field.Label>
            <Input
              value={duplicateOf}
              onChange={(e) => setDuplicateOf(e.target.value)}
              placeholder="PARSER-12"
            />
          </Field>
          <Button variant="filled" size="sm"
            disabled={busy || !duplicateOf.trim()}
            onClick={() => void run(() => markDuplicate.mutateAsync(duplicateOf.trim()))}
          >
            Confirm duplicate
          </Button>
        </InlineForm>
      ) : null}

      {error ? <FieldError>{error}</FieldError> : null}
    </div>
  );
}
