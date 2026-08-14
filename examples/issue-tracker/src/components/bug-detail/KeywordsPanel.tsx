import { useMemo, useState } from "react";
import { Button, Checkbox, Tag, Input } from "@zeroship/ui";
import { attachKeyword, createKeyword, detachKeyword, listKeywords } from "../../api";
import { deriveNamedSet } from "../activity";
import { AsyncSection } from "../StateViews";
import { errorMessage, useAsync } from "../rpc";
import type { Activity } from "../types";
import { RailDisclosure } from "./RailDisclosure";

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

  const names = [...attached];
  const summary =
    names.length === 0 ? (
      <span className="dim">none</span>
    ) : (
      <span className="rail-tags">
        {names.map((name) => (
          <Tag key={name} size="sm">
            {name}
          </Tag>
        ))}
      </span>
    );

  return (
    <RailDisclosure label="Labels" summary={summary}>
    <section className="keywords-panel">
      <div className="keyword-tags">
        {/* These two empty states say DIFFERENT things -- none attached to
            this bug, versus none defined anywhere -- and stacked as "none"
            above "No keywords defined yet." they read as one statement
            contradicting itself. Both now name their own subject. */}
        {[...attached].length === 0 ? (
          // ...but only while there is a vocabulary to have chosen from. When
          // the tracker defines no keywords at all, "No keywords on this bug"
          // states a consequence of the message directly below it, and the
          // panel says nothing twice. The distinction is real; showing both at
          // once is what was redundant.
          state.status === "ready" && state.data.length === 0 ? null : (
            <span className="dim">No keywords on this bug.</span>
          )
        ) : (
          [...attached].map((name) => (
            <Tag key={name} size="sm">
              {name}
            </Tag>
          ))
        )}
      </div>
      <AsyncSection
        state={state}
        onRetry={reloadKeywords}
        loadingLabel="Loading keywords..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No keywords have been defined for this tracker yet."
        emptyTone="inline"
      >
        {(keywords) => (
          <div className="keyword-picker">
            {keywords.map((keyword) => (
              <Checkbox
                key={keyword.id}
                size="sm"
                checked={attached.has(keyword.name)}
                disabled={busy}
                onCheckedChange={() => void toggle(keyword.id, keyword.name)}
                label={keyword.name}
                fieldClassName="keyword-option"
              />
            ))}
          </div>
        )}
      </AsyncSection>
      <div className="inline-form">
        <Input
          aria-label="New keyword name" placeholder="New keyword name"
          value={newKeyword}
          onChange={(e) => setNewKeyword(e.target.value)}
        />
        <Button variant="gray" size="small" disabled={busy || !newKeyword.trim()} onClick={() => void createAndAttach()}>
          Create + attach
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </section>
    </RailDisclosure>
  );
}
