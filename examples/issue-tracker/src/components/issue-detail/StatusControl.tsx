// The Bugzilla-fidelity centerpiece: status and resolution are always two
// separate controls, and moving to RESOLVED always requires an explicit,
// separate resolution pick before the mutation fires. Reopening and marking
// a duplicate are their own dedicated actions, not options folded into the
// status dropdown.
import { useState } from "react";
import { Select } from "../../ui/Select";
import { Button } from "../../ui/Button";
import { Field } from "../../ui/Field";
import { Input } from "../../ui/Input";
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
import { FieldError, InlineForm } from "../AppPrimitives";
import { errorMessage } from "../rpc";
import type { IssueDetail } from "../types";
import { Absent } from "./Absent";
import { RailRow } from "./RailRow";
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
    <>
      <RailSection title="Status" />
      {/* Two rail ROWS, on the rail's own tracks.
          They were a flex pair floating in a col-span-full block, so status
          and resolution were the only two facts in the column that did not
          line up with the rest -- and "Resolution Unresolved" in particular
          was two greys of the same weight side by side, where a reader could
          not tell whether the second word was the value or the back half of a
          phrase. On the grid, the label is in the label track and the value is
          in the value track, which is what says which is which. */}
      <RailRow
        label="Status"
        value={
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
        }
        action={null}
      />
      {/* A VALUE, not a disabled input. Resolution is never typed here -- it
          is chosen in the Resolve flow below, which also enforces the pairing
          with status. A greyed-out text box holding "--" says "you could edit
          this, but not right now", which is the opposite of true.
          COPY: the empty case said "Unresolved" and now says "--", the one
          token this rail uses for "no value here" (see Absent.tsx). It was the
          last field with a private word for empty, and an open status one line
          above already states that nothing is resolved. */}
      <RailRow
        label="Resolution"
        value={
          issue.resolution ? <ResolutionBadge resolution={issue.resolution} /> : <Absent />
        }
        action={null}
      />

      <div className="col-span-full mt-2 mb-1 flex flex-wrap gap-2">
        {targets.includes("RESOLVED") ? (
          <Button variant="gray" disabled={busy} onClick={() => setShowResolve((v) => !v)}>
            Resolve...
          </Button>
        ) : null}
        {currentStatus && !isOpenIssueStatus(currentStatus) ? (
          <Button variant="gray" disabled={busy} onClick={() => void run(() => reopen.mutateAsync(undefined))}>
            Reopen
          </Button>
        ) : null}
        <Button variant="gray" disabled={busy} onClick={() => setShowDuplicate((v) => !v)}>
          Mark as duplicate...
        </Button>
      </div>

      {showResolve ? (
        <InlineForm className="col-span-full">
          <Field.Root>
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
          </Field.Root>
          <Button variant="filled"
            disabled={busy}
            onClick={() => void run(() => resolve.mutateAsync(resolution))}
          >
            Confirm resolve
          </Button>
        </InlineForm>
      ) : null}

      {showDuplicate ? (
        <InlineForm className="col-span-full">
          <Field.Root>
            <Field.Label>Duplicate of</Field.Label>
            <Input
              value={duplicateOf}
              onChange={(e) => setDuplicateOf(e.target.value)}
              placeholder="PARSER-12"
            />
          </Field.Root>
          <Button variant="filled"
            disabled={busy || !duplicateOf.trim()}
            onClick={() => void run(() => markDuplicate.mutateAsync(duplicateOf.trim()))}
          >
            Confirm duplicate
          </Button>
        </InlineForm>
      ) : null}

      {error ? <FieldError className="col-span-full">{error}</FieldError> : null}
    </>
  );
}
