// The structured field builder and saved searches, rendered as a modal over
// the issue list.
//
// The QuickSearch box that used to live beside them is gone: its shorthand is
// parsed in the list's own search input now, so the feature is reachable
// without a second box. Leaving the component here exported and unrendered is
// how it became unreachable in the first place.
import { useState } from "react";
import { Button, Input, PageHeader, Select } from "@zeroship/ui";
import {
  deleteSavedSearch,
  saveSavedSearch,
  structuredSearch,
} from "../../api";
import { invalidatedBy } from "../../lib/query-keys";
import { useAppMutation, useSavedSearches } from "../../lib/queries";
import { ALL_ISSUE_COLUMNS, IssueResultsTable, type IssueColumnKey } from "../IssueResultsTable";
import { AsyncSection, ErrorState, Loading } from "../StateViews";
import { errorMessage, isUnauthenticated, toPromise } from "../rpc";
import type { Issue } from "../types";

const RESULT_COLUMNS: IssueColumnKey[] = ["id", "status", "resolution", "kind", "severity", "priority", "summary", "updated"];

type Field =
  | "id"
  | "productId"
  | "componentId"
  | "summary"
  | "kind"
  | "status"
  | "resolution"
  | "severity"
  | "priority"
  | "assigneeId"
  | "reporterId"
  | "whiteboard";

type Operator = "eq" | "ne" | "contains" | "exists";

const FIELDS: Field[] = [
  "summary",
  "kind",
  "status",
  "resolution",
  "severity",
  "priority",
  "productId",
  "componentId",
  "assigneeId",
  "reporterId",
  "whiteboard",
];

const OPERATORS: { key: Operator; label: string }[] = [
  { key: "eq", label: "is" },
  { key: "ne", label: "is not" },
  { key: "contains", label: "contains" },
  { key: "exists", label: "is set" },
];

type Condition = { id: number; field: Field; operator: Operator; value: string };

type WhereNode =
  | { op: "and"; clauses: WhereNode[] }
  | { field: Field; operator: Operator; value?: unknown };

let conditionSeq = 0;
function newCondition(): Condition {
  return { id: ++conditionSeq, field: "summary", operator: "contains", value: "" };
}

function conditionsToWhere(conditions: Condition[]): WhereNode {
  const clauses: WhereNode[] = conditions
    .filter((c) => c.operator === "exists" || c.value.trim() !== "")
    .map((c) => ({
      field: c.field,
      operator: c.operator,
      ...(c.operator === "exists" ? { value: true } : { value: c.value.trim() }),
    }));
  if (clauses.length === 0) {
    return { op: "and", clauses: [{ field: "id", operator: "exists", value: true }] };
  }
  return { op: "and", clauses };
}

export function FieldBuilder({ onResults }: { onResults: (issues: Issue[]) => void }) {
  const [conditions, setConditions] = useState<Condition[]>([newCondition()]);
  const [lastWhere, setLastWhere] = useState<WhereNode | null>(null);

  /**
   * A search READS, so it makes nothing stale -- hence the empty invalidation.
   * It is still a `useAppMutation` rather than a bare call, because it is
   * IMPERATIVE: the caller carries the rows away to render them elsewhere, so
   * there is no key the cache could own them under. Going through the one
   * layer is what keeps `busy` and `error` from being hand-rolled again here.
   */
  const search = useAppMutation(
    (where: WhereNode) => structuredSearch({ where, limit: 100 }),
    () => [],
  );

  const run = () => {
    const where = conditionsToWhere(conditions);
    setLastWhere(where);
    search.mutate(where, { onSuccess: (issues) => onResults(issues) });
  };

  return (
    <section className="field-builder">
      {/* Not "Advanced search" again. The page heading already says that, and
          the two sat one above the other -- the same duplicate-heading defect
          the issue page had. This names what the section IS: the structured
          builder, as opposed to the QuickSearch box above it. */}
      <h2>Field builder</h2>
      {conditions.map((condition, index) => (
        <div className="condition-row" key={condition.id}>
          <Select
            value={condition.field}
            aria-label="Field"
            onValueChange={(next) =>
              setConditions((cs) =>
                cs.map((c) => (c.id === condition.id ? { ...c, field: next as Field } : c)),
              )
            }
          >
            {FIELDS.map((f) => (
              <Select.Item key={f} value={f}>
                {f}
              </Select.Item>
            ))}
          </Select>
          <Select
            value={condition.operator}
            aria-label="Operator"
            onValueChange={(next) =>
              setConditions((cs) =>
                cs.map((c) => (c.id === condition.id ? { ...c, operator: next as Operator } : c)),
              )
            }
          >
            {OPERATORS.map((op) => (
              <Select.Item key={op.key} value={op.key}>
                {op.label}
              </Select.Item>
            ))}
          </Select>
          {condition.operator !== "exists" ? (
            <Input
              value={condition.value}
              onChange={(e) =>
                setConditions((cs) => cs.map((c) => (c.id === condition.id ? { ...c, value: e.target.value } : c)))
              }
              aria-label="Value" placeholder="value"
            />
          ) : null}
          <Button variant="gray" size="sm"
            disabled={conditions.length === 1}
            onClick={() => setConditions((cs) => cs.filter((_, i) => i !== index))}
          >
            Remove
          </Button>
        </div>
      ))}
      <div className="field-builder-actions">
        <Button variant="gray" size="sm" onClick={() => setConditions((cs) => [...cs, newCondition()])}>
          Add condition (AND)
        </Button>
        <Button variant="filled" size="sm" disabled={search.isPending} onClick={run}>
          {search.isPending ? "Searching..." : "Run search"}
        </Button>
      </div>
      {search.error ? <p className="field-error">{errorMessage(search.error)}</p> : null}
      <SavedSearchesPanel currentWhere={lastWhere} />
    </section>
  );
}

export function SavedSearchesPanel({ currentWhere }: { currentWhere: WhereNode | null }) {
  const savedQ = useSavedSearches();
  const [name, setName] = useState("");

  // `invalidatedBy` has no entry for saved searches -- they are the one thing
  // here that nothing else derives from -- so the prefix is named from
  // `queryKeys` directly. Still a PREFIX, so a future keyed-by-owner list is
  // covered without editing both writers.
  const save = useAppMutation(
    (args: { name: string; queryJson: WhereNode }) => saveSavedSearch(args),
    () => invalidatedBy.savedSearchChanged(),
  );
  const remove = useAppMutation(
    (id: string) => deleteSavedSearch({ id }),
    () => invalidatedBy.savedSearchChanged(),
  );
  const busy = save.isPending || remove.isPending;
  const error = save.error ?? remove.error;

  const submitSave = () => {
    if (!currentWhere || !name.trim()) return;
    // Clearing the box is the callback that SURVIVES the migration: it is form
    // state, not staleness. Refreshing the list is the mutation's own
    // invalidation now, so there is no `reload()` to thread.
    save.mutate({ name: name.trim(), queryJson: currentWhere }, { onSuccess: () => setName("") });
  };

  return (
    <div className="saved-searches">
      <h3>Saved searches</h3>
      <AsyncSection
        query={savedQ}
        loadingLabel="Loading saved searches..."
        isEmpty={(data) => data.length === 0}
        // Inline: a block empty state renders its title as an h2, which
        // inside this h3 section put an h2 under an h3 and inverted the
        // document outline. A short list in a dialog does not need a heading
        // to say it is empty.
        emptyTitle="No saved searches yet."
        emptyTone="inline"
      >
        {(rows) => (
          <ul>
            {rows.map((row) => (
              <li key={row.id}>
                {row.name}
                {row.isShared ? <span className="chip">shared</span> : null}
                <Button variant="gray" size="sm" disabled={busy} onClick={() => remove.mutate(row.id)}>
                  Delete
                </Button>
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
      <div className="inline-form">
        <Input
          aria-label="Save current search as"
          placeholder="Save current search as..."
          value={name}
          onChange={(e) => setName(e.target.value)}
          disabled={!currentWhere}
        />
        <Button variant="gray" size="sm" disabled={busy || !currentWhere || !name.trim()} onClick={submitSave}>
          Save
        </Button>
      </div>
      {error ? <p className="field-error">{errorMessage(error)}</p> : null}
    </div>
  );
}

