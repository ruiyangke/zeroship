import { useCallback, useEffect, useState } from "react";
import { Button, Field, Input } from "@zeroship/ui";

import { addSeeAlso, listSeeAlso, removeSeeAlso } from "../../api";
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
  const [links, setLinks] = useState<Awaited<ReturnType<typeof listSeeAlso>>>([]);
  // Distinct from `links.length === 0`. Seeding the list empty makes "no links"
  // and "not asked yet" the same value, so the rail asserted the issue had no
  // See Also entries for as long as the request took -- and if it failed, for
  // good. Only after this flips is an empty list a fact about the issue.
  const [loaded, setLoaded] = useState(false);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setLinks(await listSeeAlso({ issueId }));
      setLoaded(true);
      setError(null);
    } catch (err) {
      setError(errorMessage(err));
    }
  }, [issueId]);

  useEffect(() => {
    void load();
  }, [load]);

  const add = async () => {
    if (!url.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await addSeeAlso({ issueId, url: url.trim() });
      setUrl("");
      await load();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      await removeSeeAlso({ id });
      await load();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const summary =
    !loaded ? (
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
        <p className="state-hint small">No linked reports.</p>
      ) : (
        <ul className="see-also-list">
          {links.map((link) => (
            <li key={link.id}>
              <a href={link.url} target="_blank" rel="noreferrer noopener">
                {link.url}
              </a>
              <Button variant="gray" size="small"
                disabled={busy}
                onClick={() => void remove(link.id)}
              >
                Remove
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="field-row">
        <Field>
          <Field.Label>Link</Field.Label>
          <Input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://bugzilla.example.org/show_bug.cgi?id=1"
          />
        </Field>
        <Button variant="gray" size="small" disabled={busy || !url.trim()} onClick={() => void add()}>
          Add
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </section>
    </RailDisclosure>
  );
}
