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
  listSavedSearches,
  listUsers,
  quickSearch,
  saveSavedSearch,
  structuredSearch,
} from "../../api";
import { ALL_ISSUE_COLUMNS, IssueResultsTable, type IssueColumnKey } from "../IssueResultsTable";
import { AsyncSection, ErrorState, Loading } from "../StateViews";
import { errorMessage, isUnauthenticated, toPromise, useAsync } from "../rpc";
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
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastWhere, setLastWhere] = useState<WhereNode | null>(null);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const where = conditionsToWhere(conditions);
      setLastWhere(where);
      const issues = await structuredSearch({ where, limit: 100 });
      onResults(issues);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
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
          <Button variant="gray" size="small"
            disabled={conditions.length === 1}
            onClick={() => setConditions((cs) => cs.filter((_, i) => i !== index))}
          >
            Remove
          </Button>
        </div>
      ))}
      <div className="field-builder-actions">
        <Button variant="gray" size="small" onClick={() => setConditions((cs) => [...cs, newCondition()])}>
          Add condition (AND)
        </Button>
        <Button variant="filled" size="small" disabled={busy} onClick={() => void run()}>
          {busy ? "Searching..." : "Run search"}
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      <SavedSearchesPanel currentWhere={lastWhere} />
    </section>
  );
}

export function SavedSearchesPanel({ currentWhere }: { currentWhere: WhereNode | null }) {
  const { state, reload } = useAsync(() => listSavedSearches({}), []);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    if (!currentWhere || !name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await saveSavedSearch({ name: name.trim(), queryJson: currentWhere });
      setName("");
      reload();
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
      await deleteSavedSearch({ id });
      reload();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="saved-searches">
      <h3>Saved searches</h3>
      <AsyncSection
        state={state}
        onRetry={reload}
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
                <Button variant="gray" size="small" disabled={busy} onClick={() => void remove(row.id)}>
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
        <Button variant="gray" size="small" disabled={busy || !currentWhere || !name.trim()} onClick={() => void save()}>
          Save
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

