import { useCallback, useEffect, useState } from "react";

import { addSeeAlso, listSeeAlso, removeSeeAlso } from "../../api";
import { errorMessage } from "../rpc";

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
export function SeeAlsoPanel({ bugId }: { bugId: string }) {
  const [links, setLinks] = useState<Awaited<ReturnType<typeof listSeeAlso>>>([]);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setLinks(await listSeeAlso({ bugId }));
      setError(null);
    } catch (err) {
      setError(errorMessage(err));
    }
  }, [bugId]);

  useEffect(() => {
    void load();
  }, [load]);

  const add = async () => {
    if (!url.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await addSeeAlso({ bugId, url: url.trim() });
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

  return (
    <section className="see-also-panel">
      <h3>See also</h3>
      {links.length === 0 ? (
        <p className="state-hint small">No linked reports.</p>
      ) : (
        <ul className="see-also-list">
          {links.map((link) => (
            <li key={link.id}>
              <a href={link.url} target="_blank" rel="noreferrer noopener">
                {link.url}
              </a>
              <button
                type="button"
                className="btn ghost small"
                disabled={busy}
                onClick={() => void remove(link.id)}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="field-row">
        <label>
          Link
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://bugzilla.example.org/show_bug.cgi?id=1"
          />
        </label>
        <button type="button" className="btn ghost small" disabled={busy || !url.trim()} onClick={() => void add()}>
          Add
        </button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}
