import { useState } from "react";
import { NumberField } from "../../ui/NumberField";
import { Button } from "../../ui/Button";
import { Field } from "../../ui/Field";

import { castVote } from "../../api";
import { invalidatedBy } from "../../lib/query-keys";
import { useAppMutation } from "../../lib/queries";
import { FieldError, Hint } from "../AppPrimitives";
import { errorMessage } from "../rpc";
import { DetailPanel } from "./DetailPanel";

/**
 * Bugzilla voting.
 *
 * A vote carries a QUANTITY, not just a yes/no, which is why the control is a
 * number and `voteCount` is the sum rather than a row count. Voting is off
 * unless the product sets a budget, so the panel says so instead of offering a
 * control that always fails.
 */
export function VotesPanel({
  issueId,
  voteCount,
  maxVotesPerIssue,
  votingEnabled,
}: {
  issueId: string;
  voteCount: number;
  maxVotesPerIssue: number;
  votingEnabled: boolean;
}) {
  const [count, setCount] = useState(1);
  const [error, setError] = useState<string | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  // A vote moves both this issue and the signed-in account's vote list. The
  // issue prefix also reaches every report total derived from it.
  const vote = useAppMutation(
    (nextCount: number) => castVote({ issueId, count: nextCount }),
    () => [
      ...invalidatedBy.issueChanged(issueId),
      ...invalidatedBy.voteChanged(),
    ],
  );

  const submit = async () => {
    setError(null);
    try {
      const result = await vote.mutateAsync(count);
      // The server reports whether this vote crossed votesToConfirm. Surfacing
      // it matters: the issue's status changed as a side effect of voting, and a
      // silent status change is the kind of thing users file bugs about.
      setConfirmed(result.confirmed);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <DetailPanel locator="votes-panel" title="Votes">
      <p className="mb-2">
        <strong>{voteCount}</strong> {voteCount === 1 ? "vote" : "votes"}
      </p>
      {!votingEnabled ? (
        <Hint>Voting is not enabled for this product.</Hint>
      ) : (
        <>
          <Field.Root>
                        {/* The design system's NumberField, deliberately.
                This was briefly swapped for a native <input type="number">
                because the voting spec's `.fill("2")` produced "12" -- but
                the component is fine and the harness was the problem.
                NumberField wraps Base UI's formatted text input, which parses
                `input` events and re-renders from its own state, so assigning
                `.value` directly leaves the two out of sync. Typing works,
                which is what a user does and what the spec now does.
                Swapping the component would have let a test-runner quirk
                decide what the app is built from. */}
            <Field.Label>My votes</Field.Label>
            <NumberField
              min={0}
              max={maxVotesPerIssue > 0 ? maxVotesPerIssue : undefined}
              value={count}
              onValueChange={(next) => setCount(Math.max(0, next ?? 0))}
            />
          </Field.Root>
          <Button variant="gray" disabled={vote.isPending} onClick={() => void submit()}>
            {vote.isPending ? "Voting..." : "Vote"}
          </Button>
          {maxVotesPerIssue > 0 ? (
            <Hint>At most {maxVotesPerIssue} on this issue.</Hint>
          ) : null}
          {confirmed ? (
            <Hint>This issue was confirmed by reaching the vote threshold.</Hint>
          ) : null}
          {error ? <FieldError>{error}</FieldError> : null}
        </>
      )}
    </DetailPanel>
  );
}
