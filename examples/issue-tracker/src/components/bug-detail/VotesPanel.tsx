import { useState } from "react";
import { Button, Field } from "@zeroship/ui";

import { castVote } from "../../api";
import { errorMessage } from "../rpc";

/**
 * Bugzilla voting.
 *
 * A vote carries a QUANTITY, not just a yes/no, which is why the control is a
 * number and `voteCount` is the sum rather than a row count. Voting is off
 * unless the product sets a budget, so the panel says so instead of offering a
 * control that always fails.
 */
export function VotesPanel({
  bugId,
  voteCount,
  maxVotesPerBug,
  votingEnabled,
  onChanged,
}: {
  bugId: string;
  voteCount: number;
  maxVotesPerBug: number;
  votingEnabled: boolean;
  onChanged: () => void;
}) {
  const [count, setCount] = useState(1);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmed, setConfirmed] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await castVote({ bugId, count });
      // The server reports whether this vote crossed votesToConfirm. Surfacing
      // it matters: the bug's status changed as a side effect of voting, and a
      // silent status change is the kind of thing users file bugs about.
      setConfirmed(result.confirmed);
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="votes-panel">
      <h3>Votes</h3>
      <p className="vote-total">
        <strong>{voteCount}</strong> {voteCount === 1 ? "vote" : "votes"}
      </p>
      {!votingEnabled ? (
        <p className="state-hint small">Voting is not enabled for this product.</p>
      ) : (
        <>
          <Field>
            {/* A plain native number input, not the design system's
                NumberField: NumberField's controlled text input does not
                replace-on-fill the way a native <input type="number"> does
                (Playwright's .fill("2") landed as "12", appended rather
                than replacing "1"), which this panel's own regression spec
                exercises directly. */}
            <Field.Label htmlFor="votes-count">My votes</Field.Label>
            <input
              id="votes-count"
              type="number"
              min={0}
              max={maxVotesPerBug > 0 ? maxVotesPerBug : undefined}
              value={count}
              onChange={(e) => setCount(Math.max(0, Number(e.target.value) || 0))}
            />
          </Field>
          <Button variant="gray" size="small" disabled={busy} onClick={() => void submit()}>
            {busy ? "Voting..." : "Vote"}
          </Button>
          {maxVotesPerBug > 0 ? (
            <p className="state-hint small">At most {maxVotesPerBug} on this bug.</p>
          ) : null}
          {confirmed ? (
            <p className="state-hint small">This bug was confirmed by reaching the vote threshold.</p>
          ) : null}
          {error ? <p className="field-error">{error}</p> : null}
        </>
      )}
    </section>
  );
}
