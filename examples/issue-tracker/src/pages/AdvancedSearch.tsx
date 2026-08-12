import { useState } from "react";
import { Select } from "@zeroship/ui";
import {
  deleteSavedSearch,
  listSavedSearches,
  listUsers,
  quickSearch,
  saveSavedSearch,
  structuredSearch,
} from "../api";
import { ALL_BUG_COLUMNS, BugResultsTable, type BugColumnKey } from "../components/BugResultsTable";
import { AsyncSection, ErrorState, Loading } from "../components/StateViews";
import { errorMessage, isUnauthenticated, toPromise, useAsync } from "../components/rpc";
import type { Bug } from "../components/types";

const RESULT_COLUMNS: BugColumnKey[] = ["id", "status", "resolution", "severity", "priority", "summary", "updated"];

type Field =
  | "id"
  | "productId"
  | "componentId"
  | "summary"
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

function QuickSearchBox() {
  const [text, setText] = useState("");
  const [result, setResult] = useState<
    | { status: "idle" }
    | { status: "loading" }
    | { status: "error"; error: unknown }
    | { status: "ready"; data: Awaited<ReturnType<typeof quickSearch>> }
  >({ status: "idle" });

  const run = async () => {
    if (!text.trim()) return;
    setResult({ status: "loading" });
    try {
      const data = await quickSearch({ text: text.trim(), limit: 50 });
      setResult({ status: "ready", data });
    } catch (error) {
      setResult({ status: "error", error });
    }
  };

  return (
    <section className="quicksearch">
      <h2>QuickSearch</h2>
      <p className="state-hint small">
        e.g. <code>P1 @alice comp:parser</code> -- priority, assignee, and component shorthand in one box.
      </p>
      <form
        className="inline-form"
        onSubmit={(e) => {
          e.preventDefault();
          void run();
        }}
      >
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder="P1 @alice comp:parser" />
        <button type="submit" className="btn primary small">
          Search
        </button>
      </form>
      {result.status === "loading" ? <Loading label="Searching..." /> : null}
      {result.status === "error" ? <ErrorState error={result.error} onRetry={run} /> : null}
      {result.status === "ready" ? (
        <>
          <div className="quicksearch-clauses">
            {result.data.clauses.map((clause, i) => (
              <span key={i} className="chip">
                {clause.field}: {clause.value}
              </span>
            ))}
          </div>
          {result.data.bugs.length === 0 ? (
            <p className="state-hint small">No bugs match.</p>
          ) : (
            <BugResultsTable bugs={result.data.bugs} columns={RESULT_COLUMNS} />
          )}
        </>
      ) : null}
    </section>
  );
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

function FieldBuilder({ onResults }: { onResults: (bugs: Bug[]) => void }) {
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
      const bugs = await structuredSearch({ where, limit: 100 });
      onResults(bugs);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="field-builder">
      <h2>Advanced search</h2>
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
            <input
              value={condition.value}
              onChange={(e) =>
                setConditions((cs) => cs.map((c) => (c.id === condition.id ? { ...c, value: e.target.value } : c)))
              }
              placeholder="value"
            />
          ) : null}
          <button
            type="button"
            className="btn ghost small"
            disabled={conditions.length === 1}
            onClick={() => setConditions((cs) => cs.filter((_, i) => i !== index))}
          >
            Remove
          </button>
        </div>
      ))}
      <div className="field-builder-actions">
        <button type="button" className="btn ghost small" onClick={() => setConditions((cs) => [...cs, newCondition()])}>
          Add condition (AND)
        </button>
        <button type="button" className="btn primary small" disabled={busy} onClick={() => void run()}>
          {busy ? "Searching..." : "Run search"}
        </button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      <SavedSearchesPanel currentWhere={lastWhere} />
    </section>
  );
}

function SavedSearchesPanel({ currentWhere }: { currentWhere: WhereNode | null }) {
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
        emptyTitle="No saved searches yet."
      >
        {(rows) => (
          <ul>
            {rows.map((row) => (
              <li key={row.id}>
                {row.name}
                {row.isShared ? <span className="chip">shared</span> : null}
                <button type="button" className="btn ghost small" disabled={busy} onClick={() => void remove(row.id)}>
                  Delete
                </button>
              </li>
            ))}
          </ul>
        )}
      </AsyncSection>
      <div className="inline-form">
        <input
          placeholder="Save current search as..."
          value={name}
          onChange={(e) => setName(e.target.value)}
          disabled={!currentWhere}
        />
        <button type="button" className="btn ghost small" disabled={busy || !currentWhere || !name.trim()} onClick={() => void save()}>
          Save
        </button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
    </div>
  );
}

export function AdvancedSearchPage() {
  const [results, setResults] = useState<Bug[] | null>(null);
  const usersQ = useAsync(() => toPromise(listUsers({ limit: 1 })).catch(() => []), []);

  const authGate = usersQ.state.status === "error" && isUnauthenticated(usersQ.state.error);

  return (
    <div className="page advanced-search-page">
      <h1>Advanced search</h1>
      <QuickSearchBox />
      {authGate ? (
        <p className="state-hint">Sign in to use the structured field builder and saved searches.</p>
      ) : (
        <>
          <FieldBuilder onResults={setResults} />
          {results ? (
            <section>
              <h2>Results ({results.length})</h2>
              {results.length === 0 ? (
                <p className="state-hint small">No bugs match this search.</p>
              ) : (
                <BugResultsTable
                  bugs={results}
                  columns={ALL_BUG_COLUMNS.map((c) => c.key).filter((c) => c !== "reporter")}
                />
              )}
            </section>
          ) : null}
        </>
      )}
    </div>
  );
}
