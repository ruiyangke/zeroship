import { useMemo, useState } from "react";
import { attachKeyword, createKeyword, detachKeyword, listKeywords } from "../../api";
import { deriveNamedSet } from "../activity";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import type { Activity } from "../types";

export function KeywordsPanel({
  bugId,
  activities,
  onChanged,
}: {
  bugId: string;
  activities: readonly Activity[];
  onChanged: () => void;
}) {
  const { state, reload: reloadKeywords } = useAsync(() => listKeywords({}), []);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [newKeyword, setNewKeyword] = useState("");

  const attached = useMemo(() => new Set(deriveNamedSet(activities, "keywords")), [activities]);

  const toggle = async (keywordId: string, keywordName: string) => {
    setBusy(true);
    setError(null);
    try {
      if (attached.has(keywordName)) {
        await detachKeyword({ bugId, keywordId });
      } else {
        await attachKeyword({ bugId, keywordId });
      }
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const createAndAttach = async () => {
    const name = newKeyword.trim();
    if (!name) return;
    setBusy(true);
    setError(null);
    try {
      const keyword = await createKeyword({ name });
      await attachKeyword({ bugId, keywordId: keyword.id });
      setNewKeyword("");
      reloadKeywords();
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="keywords-panel">
      <h3>Keywords</h3>
      <div className="keyword-tags">
        {[...attached].length === 0 ? (
          <span className="dim">none</span>
        ) : (
          [...attached].map((name) => <span key={name} className="chip">{name}</span>)
        )}
      </div>
      <AsyncSection
        state={state}
        onRetry={reloadKeywords}
        loadingLabel="Loading keywords..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No keywords defined yet."
      >
        {(keywords) => (
          <div className="keyword-picker">
            {keywords.map((keyword) => (
              <label key={keyword.id} className="keyword-option">
                <input
                  type="checkbox"
                  checked={attached.has(keyword.name)}
                  disabled={busy}
                  onChange={() => void toggle(keyword.id, keyword.name)}
                />
                {keyword.name}
              </label>
            ))}
          </div>
        )}
      </AsyncSection>
      <div className="inline-form">
        <input
          placeholder="New keyword name"
          value={newKeyword}
          onChange={(e) => setNewKeyword(e.target.value)}
        />
        <button type="button" className="btn ghost small" disabled={busy || !newKeyword.trim()} onClick={() => void createAndAttach()}>
          Create + attach
        </button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </section>
  );
}
