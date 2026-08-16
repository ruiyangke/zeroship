import { useMemo, useState } from "react";
import { Checkbox } from "../../ui/Checkbox";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { Tag } from "../../ui/Tag";
import { attachKeyword, createKeyword, detachKeyword } from "../../api";
import { FieldError, InlineForm, Muted } from "../AppPrimitives";
import { deriveNamedSet } from "../activity";
import { AsyncSection } from "../StateViews";
import { errorMessage } from "../rpc";
import { useAppMutation, useKeywords } from "../../lib/queries";
import { invalidatedBy } from "../../lib/query-keys";
import type { Activity } from "../types";
import { RailDisclosure } from "./RailDisclosure";
import { Absent } from "./Absent";

export function KeywordsPanel({
  issueId,
  activities,
}: {
  issueId: string;
  activities: readonly Activity[];
}) {
  const keywordsQ = useKeywords();
  const [error, setError] = useState<string | null>(null);
  const [newKeyword, setNewKeyword] = useState("");

  const attached = useMemo(() => new Set(deriveNamedSet(activities, "keywords")), [activities]);

  // Attaching or detaching is a relation change, so it drops the issue detail
  // too -- which is where `activities`, and therefore the attached set above,
  // comes from. That is the link the removed `onChanged` prop used to carry
  // by hand.
  const toggleKeyword = useAppMutation(
    ({ keywordId, attach }: { keywordId: string; attach: boolean }) =>
      attach ? attachKeyword({ issueId, keywordId }) : detachKeyword({ issueId, keywordId }),
    () => invalidatedBy.relationsChanged(issueId),
  );

  // Creating one also changes the tracker's VOCABULARY, which is a different
  // question from this issue's relations: `keywords.list` is shared by every
  // issue, so it is named here in addition to the relation keys.
  const createAndAttachKeyword = useAppMutation(
    async (name: string) => {
      const keyword = await createKeyword({ name });
      await attachKeyword({ issueId, keywordId: keyword.id });
    },
    () => invalidatedBy.keywordCreated(issueId),
  );
  const busy = toggleKeyword.isPending || createAndAttachKeyword.isPending;

  const toggle = async (keywordId: string, keywordName: string) => {
    setError(null);
    try {
      await toggleKeyword.mutateAsync({ keywordId, attach: !attached.has(keywordName) });
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const createAndAttach = async () => {
    const name = newKeyword.trim();
    if (!name) return;
    setError(null);
    try {
      await createAndAttachKeyword.mutateAsync(name);
      setNewKeyword("");
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const names = [...attached];
  const summary =
    names.length === 0 ? (
      <Absent />
    ) : (
      <span className="flex flex-wrap gap-1">
        {names.map((name) => (
          <Tag key={name} size="sm">
            {name}
          </Tag>
        ))}
      </span>
    );

  return (
    <RailDisclosure label="Labels" summary={summary}>
    <section>
      <div className="mb-2">
        {/* These two empty states say DIFFERENT things -- none attached to
            this issue, versus none defined anywhere -- and stacked as "none"
            above "No keywords defined yet." they read as one statement
            contradicting itself. Both now name their own subject. */}
        {[...attached].length === 0 ? (
          // ...but only while there is a vocabulary to have chosen from. When
          // the tracker defines no keywords at all, "No keywords on this issue"
          // states a consequence of the message directly below it, and the
          // panel says nothing twice. The distinction is real; showing both at
          // once is what was redundant. `data?.length === 0` is only true once
          // the list has ARRIVED and is empty, so an in-flight query does not
          // silence this line.
          keywordsQ.data?.length === 0 ? null : (
            <Muted>No keywords on this issue.</Muted>
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
        query={keywordsQ}
        loadingLabel="Loading keywords..."
        isEmpty={(data) => data.length === 0}
        emptyTitle="No keywords have been defined for this tracker yet."
        emptyTone="inline"
      >
        {(keywords) => (
          <div className="mb-2 flex flex-wrap gap-2">
            {keywords.map((keyword) => (
              <Checkbox
                key={keyword.id}
                size="sm"
                checked={attached.has(keyword.name)}
                disabled={busy}
                onCheckedChange={() => void toggle(keyword.id, keyword.name)}
                label={keyword.name}
                fieldClassName="flex-row items-center gap-1 text-base text-ink-secondary"
              />
            ))}
          </div>
        )}
      </AsyncSection>
      <InlineForm>
        <Input
          aria-label="New keyword name" placeholder="New keyword name"
          value={newKeyword}
          onChange={(e) => setNewKeyword(e.target.value)}
        />
        <Button variant="gray" disabled={busy || !newKeyword.trim()} onClick={() => void createAndAttach()}>
          Create + attach
        </Button>
      </InlineForm>
      {error ? <FieldError>{error}</FieldError> : null}
    </section>
    </RailDisclosure>
  );
}
