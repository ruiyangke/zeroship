import { useState } from "react";
import { Button, Field, Input } from "@zeroship/ui";

import { addSeeAlso, removeSeeAlso } from "../../api";
import { useAppMutation, useSeeAlso } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
import { FieldError, FieldRow, Hint } from "../AppPrimitives";
import { RailDisclosure } from "./RailDisclosure";
import { errorMessage } from "../rpc";
import { Absent, Pending } from "./Absent";

/**
 * Bugzilla's See Also: links to the same issue in other trackers.
 *
 * The server validates the scheme -- `javascript:` in an href is script
 * execution, not a link -- so the field is rendered as a real anchor here. The
 * client does not re-validate: one authority for a rule beats two that can
 * disagree, and the server's is the one that cannot be bypassed.
 *
 * `rel="noreferrer noopener"` because these point at trackers this app knows
 * nothing about.
 */
export function SeeAlsoPanel({ issueId }: { issueId: string }) {
  const linksQ = useSeeAlso(issueId);
  const links = linksQ.data ?? [];
  const [url, setUrl] = useState("");
  const [error, setError] = useState<string | null>(null);

  const addLink = useAppMutation(
    (nextUrl: string) => addSeeAlso({ issueId, url: nextUrl }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const removeLink = useAppMutation(
    (id: string) => removeSeeAlso({ id }),
    () => invalidatedBy.relationsChanged(issueId),
  );
  const busy = addLink.isPending || removeLink.isPending;

  const add = async () => {
    if (!url.trim()) return;
    setError(null);
    try {
      await addLink.mutateAsync(url.trim());
      setUrl("");
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const remove = async (id: string) => {
    setError(null);
    try {
      await removeLink.mutateAsync(id);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  // Distinct from `links.length === 0`. The query keeps data undefined until
  // the first successful answer, so "no links" and "not asked yet" remain
  // different values without a second `loaded` flag. Only an answered empty
  // array is a fact about the issue; pending or failed first reads stay a
  // Skeleton rather than asserting absence.
  const summary =
    linksQ.data === undefined ? (
      <Pending width="5rem" />
    ) : links.length === 0 ? (
      <Absent />
    ) : (
      <>{links.length} linked</>
    );

  return (
    <RailDisclosure label="See also" summary={summary} action="Add">
    <section className="see-also-panel">
      {links.length === 0 ? (
        <Hint>No linked reports.</Hint>
      ) : (
        <ul className="m-0 list-none p-0">
          {links.map((link) => (
            <li key={link.id}>
              <a href={link.url} target="_blank" rel="noreferrer noopener">
                {link.url}
              </a>
              <Button variant="gray" size="sm"
                disabled={busy}
                onClick={() => void remove(link.id)}
              >
                Remove
              </Button>
            </li>
          ))}
        </ul>
      )}
      <FieldRow>
        <Field>
          <Field.Label>Link</Field.Label>
          <Input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://bugzilla.example.org/show_bug.cgi?id=1"
          />
        </Field>
        <Button variant="gray" size="sm" disabled={busy || !url.trim()} onClick={() => void add()}>
          Add
        </Button>
      </FieldRow>
      {error || linksQ.isError ? (
        <FieldError>{error ?? errorMessage(linksQ.error)}</FieldError>
      ) : null}
    </section>
    </RailDisclosure>
  );
}
