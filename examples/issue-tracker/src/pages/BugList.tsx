import { useState } from "react";
import { PageHeader } from "@zeroship/ui";
import { listProducts, searchBugs } from "../api";
import {
  ALL_BUG_COLUMNS,
  BugResultsTable,
  type BugColumnKey,
} from "../components/BugResultsTable";
import { AsyncSection } from "../components/StateViews";
import { useAsync } from "../components/rpc";
import type { Bug } from "../components/types";
import { BUG_STATUSES, type BugStatus } from "../lib/workflow";
import { BUG_PRIORITIES, BUG_SEVERITIES } from "../lib/quicksearch";

const DEFAULT_COLUMNS: BugColumnKey[] = [
  "id",
  "status",
  "resolution",
  "severity",
  "priority",
  "product",
  "summary",
  "assignee",
  "updated",
];

const COLUMNS_STORAGE_KEY = "issue-tracker:list-columns";
const PAGE_SIZE = 25;

function loadColumns(): BugColumnKey[] {
  try {
    const raw = window.localStorage.getItem(COLUMNS_STORAGE_KEY);
    if (!raw) return DEFAULT_COLUMNS;
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return DEFAULT_COLUMNS;
    const valid = parsed.filter((c): c is BugColumnKey =>
      ALL_BUG_COLUMNS.some((col) => col.key === c),
    );
    return valid.length > 0 ? valid : DEFAULT_COLUMNS;
  } catch {
    return DEFAULT_COLUMNS;
  }
}

type SortBy = "created_at" | "updated_at" | "priority" | "severity" | "status";

export function BugListPage() {
  const [text, setText] = useState("");
  const [status, setStatus] = useState<BugStatus | "">("");
  const [severity, setSeverity] = useState<(typeof BUG_SEVERITIES)[number] | "">("");
  const [priority, setPriority] = useState<(typeof BUG_PRIORITIES)[number] | "">("");
  const [productId, setProductId] = useState("");
  const [sortBy, setSortBy] = useState<SortBy>("updated_at");
  const [sortDirection, setSortDirection] = useState<1 | -1>(-1);
  const [offset, setOffset] = useState(0);
  const [columns, setColumns] = useState<BugColumnKey[]>(loadColumns);
  const [pickerOpen, setPickerOpen] = useState(false);

  const productsQ = useAsync(() => listProducts({}), []);

  const { state, reload } = useAsync(
    () =>
      searchBugs({
        text: text.trim() || undefined,
        status: status || undefined,
        severity: severity || undefined,
        priority: priority || undefined,
        productId: productId || undefined,
        sortBy,
        sortDirection,
        limit: PAGE_SIZE,
        offset,
      }),
    [text, status, severity, priority, productId, sortBy, sortDirection, offset],
  );


  const toggleColumn = (key: BugColumnKey) => {
    setColumns((prev) => {
      const next = prev.includes(key) ? prev.filter((c) => c !== key) : [...prev, key];
      const ordered = ALL_BUG_COLUMNS.map((c) => c.key).filter((c) => next.includes(c));
      window.localStorage.setItem(COLUMNS_STORAGE_KEY, JSON.stringify(ordered));
      return ordered;
    });
  };

  const resetFilters = () => {
    setText("");
    setStatus("");
    setSeverity("");
    setPriority("");
    setProductId("");
    setOffset(0);
  };

  return (
    <div className="page bug-list-page">
      {/* No page-level "New bug": the shell header carries it on every
          page, and two of them side by side was the first thing the
          screenshot showed. */}
      {/* Compound parts, not a `title` prop -- PageHeader is a container and
          passing title rendered nothing at all, which reads as a missing
          heading rather than a wrong API. */}
      <PageHeader>
        <PageHeader.Title>Bugs</PageHeader.Title>
      </PageHeader>

      <form
        className="filter-bar"
        onSubmit={(e) => {
          e.preventDefault();
          setOffset(0);
          reload();
        }}
      >
        <input
          placeholder="Search summary, whiteboard, URL..."
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        <select value={status} onChange={(e) => setStatus(e.target.value as BugStatus | "")}>
          <option value="">Any status</option>
          {BUG_STATUSES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <select
          value={severity}
          onChange={(e) => setSeverity(e.target.value as (typeof BUG_SEVERITIES)[number] | "")}
        >
          <option value="">Any severity</option>
          {BUG_SEVERITIES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <select
          value={priority}
          onChange={(e) => setPriority(e.target.value as (typeof BUG_PRIORITIES)[number] | "")}
        >
          <option value="">Any priority</option>
          {BUG_PRIORITIES.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
        <select value={productId} onChange={(e) => setProductId(e.target.value)}>
          <option value="">Any product</option>
          {productsQ.state.status === "ready" &&
            productsQ.state.data.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
        </select>
        <button type="submit" className="btn ghost small">
          Apply
        </button>
        <button type="button" className="btn ghost small" onClick={resetFilters}>
          Reset
        </button>
        <div className="column-picker">
          <button
            type="button"
            className="btn ghost small"
            onClick={() => setPickerOpen((v) => !v)}
          >
            Columns
          </button>
          {pickerOpen ? (
            <div className="column-picker-menu">
              {ALL_BUG_COLUMNS.map((col) => (
                <label key={col.key}>
                  <input
                    type="checkbox"
                    checked={columns.includes(col.key)}
                    disabled={col.key === "summary"}
                    onChange={() => toggleColumn(col.key)}
                  />
                  {col.label}
                </label>
              ))}
            </div>
          ) : null}
        </div>
      </form>

      <div className="sort-bar">
        <span>Sort:</span>
        <select value={sortBy} onChange={(e) => setSortBy(e.target.value as SortBy)}>
          <option value="updated_at">Updated</option>
          <option value="created_at">Created</option>
          <option value="priority">Priority</option>
          <option value="severity">Severity</option>
          <option value="status">Status</option>
        </select>
        <button
          type="button"
          className="btn ghost small"
          onClick={() => setSortDirection((d) => (d === 1 ? -1 : 1))}
        >
          {sortDirection === 1 ? "Ascending" : "Descending"}
        </button>
      </div>

      <AsyncSection
        state={state}
        onRetry={reload}
        loadingLabel="Loading bugs..."
        isEmpty={(data) => data.length === 0}
        emptyTitle={
          text || status || severity || priority || productId
            ? "No bugs match these filters."
            : "No bugs visible."
        }
        emptyHint={
          text || status || severity || priority || productId
            ? "Try widening the filters."
            : "Either nothing has been filed yet, or there is no signed-in identity with visibility into any product."
        }
      >
        {(bugs: Bug[]) => (
          <>
            <BugResultsTable
              bugs={bugs}
              columns={columns}
            />
            <div className="pager">
              <button
                type="button"
                className="btn ghost small"
                disabled={offset === 0}
                onClick={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
              >
                Previous
              </button>
              <span>
                {offset + 1}-{offset + bugs.length}
              </span>
              <button
                type="button"
                className="btn ghost small"
                disabled={bugs.length < PAGE_SIZE}
                onClick={() => setOffset((o) => o + PAGE_SIZE)}
              >
                Next
              </button>
            </div>
          </>
        )}
      </AsyncSection>
    </div>
  );
}
