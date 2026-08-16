// Flags are READ from the server via flags.list, not reconstructed.
//
// This panel used to derive each flag's current value by replaying the issue's
// activity log. That was exact only for non-multiplicable flag types -- several
// live flags of one type collapse to whichever was written last -- and it left
// "Clear" unusable, because clearing needs a flag id and the id was only ever
// returned by flags.set. A flag set in an earlier session could never be
// cleared at all. flags.list now returns the real rows, ids included, so both
// problems are gone and the caveat that used to sit at the bottom of this panel
// is deleted rather than reworded.
import { useState } from "react";
import { Select } from "@zeroship/ui";
import { Button } from "../../ui/Button";

import { clearFlag, setFlag } from "../../api";
import { invalidatedBy } from "../../lib/query-keys";
import { useAppMutation, useFlags } from "../../lib/queries";
import { FieldError, Hint, Muted } from "../AppPrimitives";
import { errorMessage } from "../rpc";
import type { FlagType } from "../types";
import { UserPicker } from "../UserPicker";
import { Absent } from "./Absent";
import { DetailPanel } from "./DetailPanel";
import { RailList } from "./RailList";
import { Badge } from "../../ui/Badge";

type FlagStatus = "+" | "-" | "?";
const FLAG_STATUSES: FlagStatus[] = ["+", "-", "?"];

type LiveFlag = NonNullable<ReturnType<typeof useFlags>["data"]>["onIssue"][number];

function FlagRow({
  issueId,
  flagType,
  live,
}: {
  issueId: string;
  flagType: FlagType;
  live: readonly LiveFlag[];
}) {
  const [status, setStatus] = useState<FlagStatus>("+");
  const [requesteeId, setRequesteeId] = useState<string | null>(null);
  const [pickingRequestee, setPickingRequestee] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Flags have three observable effects: the live flag relation changes, a
  // `?` flag moves the account request list, and the issue and report totals
  // can change with it. The cache owns those refreshes now, so neither this
  // row nor its parent needs a callback.
  const applyFlag = useAppMutation(
    ({ nextStatus, nextRequesteeId }: {
      nextStatus: FlagStatus;
      nextRequesteeId: string | undefined;
    }) =>
      setFlag({
        flagTypeId: flagType.id,
        issueId,
        status: nextStatus,
        requesteeId: nextRequesteeId,
      }),
    () => [
      ...invalidatedBy.issueChanged(issueId),
      ...invalidatedBy.flagChanged(issueId),
    ],
  );
  const clearLiveFlag = useAppMutation(
    (id: string) => clearFlag({ id }),
    () => [
      ...invalidatedBy.issueChanged(issueId),
      ...invalidatedBy.flagChanged(issueId),
    ],
  );
  const busy = applyFlag.isPending || clearLiveFlag.isPending;

  // Every live flag of this type, not just the last one. A multiplicable type
  // legitimately has several at once, and collapsing them was the old bug.
  const mine = live.filter((entry) => entry.flag.flagTypeId === flagType.id);

  const apply = async () => {
    setError(null);
    try {
      await applyFlag.mutateAsync({
        nextStatus: status,
        nextRequesteeId: requesteeId ?? undefined,
      });
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const clear = async (id: string) => {
    setError(null);
    try {
      await clearLiveFlag.mutateAsync(id);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <li className="flex flex-wrap items-center gap-2 text-md">
      <span className="min-w-22 font-semibold">{flagType.name}</span>
      {mine.length === 0 ? (
        <Absent />
      ) : (
        mine.map((entry) => (
          <span key={entry.flag.id}>
            {/* A real Badge carrying the MEANING. These were spans with
                flag-current-plus / -minus / -question class names that appear
                nowhere in the stylesheet, so a granted flag, a denied one and
                an open request all rendered identically -- the three states
                the whole feature exists to distinguish. */}
            <Badge
              intent={
                entry.flag.status === "+"
                  ? "success"
                  : entry.flag.status === "-"
                    ? "danger"
                    : "warning"
              }
              variant="soft"
              size="sm"
            >
              {entry.flag.status}
            </Badge>
            {entry.requestee ? <Muted> to {entry.requestee.name}</Muted> : null}
            {/* Clearing works for ANY live flag now, not only one this panel
                set, because the id comes from the server rather than from a
                setFlag response held in component state. */}
            <Button variant="gray"
              disabled={busy}
              onClick={() => void clear(entry.flag.id)}
            >
              Clear
            </Button>
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
        <span>
          {requesteeId ? (
            <Muted>requestee: {requesteeId}</Muted>
          ) : (
            <Button variant="gray"
              onClick={() => setPickingRequestee((v) => !v)}
            >
              set requestee
            </Button>
          )}
        </span>
      ) : null}
      <Button variant="gray" disabled={busy} onClick={() => void apply()}>
        Set
      </Button>
      {pickingRequestee ? (
        <UserPicker
          onPick={(u) => {
            setRequesteeId(u.id);
            setPickingRequestee(false);
          }}
        />
      ) : null}
      {error ? <FieldError>{error}</FieldError> : null}
    </li>
  );
}

export function FlagsPanel({
  issueId,
  flagTypes,
}: {
  issueId: string;
  flagTypes: readonly FlagType[] | null;
}) {
  const flagsQ = useFlags(issueId);
  const live = flagsQ.data?.onIssue ?? [];

  const issueFlagTypes = flagTypes?.filter((t) => t.targetType === "issue") ?? [];
  return (
    <DetailPanel locator="flags-panel" title="Flags">
      {flagTypes === null ? (
        <Hint>Sign in to see and set flags for this product.</Hint>
      ) : issueFlagTypes.length === 0 ? (
        <Hint>This product defines no issue-level flag types.</Hint>
      ) : (
        <RailList roomy>
          {issueFlagTypes.map((flagType) => (
            <FlagRow
              key={flagType.id}
              issueId={issueId}
              flagType={flagType}
              live={live}
            />
          ))}
        </RailList>
      )}
      {/* Surfaced rather than swallowed: an unreadable flag list rendering as
          "not set" would claim, wrongly, that the issue carries no flags. */}
      {flagsQ.isError ? (
        <FieldError>Could not load flags: {errorMessage(flagsQ.error)}</FieldError>
      ) : null}
    </DetailPanel>
  );
}
