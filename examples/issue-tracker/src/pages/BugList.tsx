import { useState } from "react";
import { Button, Card, Checkbox, Cluster, FilterBar, PageHeader, Select } from "@zeroship/ui";
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

// The table speaks in COLUMN keys and the server in sort fields; these map
// between them. Kept as a pair rather than reusing one vocabulary because the
// id column sorts by creation time, which no single name describes.
const COLUMN_SORT: Record<string, SortBy> = {
  id: "created_at",
  status: "status",
  severity: "severity",
  priority: "priority",
  updated: "updated_at",
};
const SORT_COLUMN: Record<SortBy, string> = {
  created_at: "id",
  status: "status",
  severity: "severity",
  priority: "priority",
  updated_at: "updated",
};

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

  // Applying resets to the first page. Staying on page 4 of the old result set
  // while the filter changes shows an empty table for a query that matched
  // plenty, which reads as "no results" rather than "you are past the end".
  const applyFilters = () => {
    setOffset(0);
    reload();
  };

  /**
   * The applied filters, as removable chips.
   *
   * Four permanently visible dropdowns told you which filters EXIST, never
   * which are ON: a list narrowed to one product looked exactly like a list
   * with nothing set, and the only way to tell was to read every control. A
   * chip appears when a filter is applied and removing it clears that one.
   */
  const activeFilters = [
    status ? { id: "status", label: `Status: ${status}`, onRemove: () => setStatus("") } : null,
    severity
      ? { id: "severity", label: `Severity: ${severity}`, onRemove: () => setSeverity("") }
      : null,
    priority
      ? { id: "priority", label: `Priority: ${priority}`, onRemove: () => setPriority("") }
      : null,
    productId
      ? {
          id: "product",
          label: `Product: ${
            productsQ.state.status === "ready"
              ? productsQ.state.data.find((p) => p.id === productId)?.name ?? productId
              : productId
          }`,
          onRemove: () => setProductId(""),
        }
      : null,
  ].filter((chip): chip is NonNullable<typeof chip> => chip !== null);

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

      {/* FilterBar owns the row: a real search Input, the applied filters as
          removable chips, and the pickers in the controls slot. The chips are
          the point -- four always-visible dropdowns showed which filters
          EXIST, never which are ON, so a narrowed list looked identical to an
          empty one. */}
      <FilterBar
        search={text}
        onSearchChange={setText}
        searchPlaceholder="Search summary, whiteboard, URL..."
        activeFilters={activeFilters}
        onClearFilters={activeFilters.length > 0 ? resetFilters : undefined}
        actions={
          <Cluster gap={2} align="center">
            <Button variant="gray" size="small" onClick={applyFilters}>
              Apply
            </Button>
            <Button
              variant="plain"
              size="small"
              onClick={() => setPickerOpen((open) => !open)}
              aria-expanded={pickerOpen}
            >
              Columns
            </Button>
          </Cluster>
        }
      >
        <Select value={status} onValueChange={(v) => setStatus((v as BugStatus) ?? "")} placeholder="Any status" className="filter-select">
          {BUG_STATUSES.map((s) => (
            <Select.Item key={s} value={s}>
              {s}
            </Select.Item>
          ))}
        </Select>
        <Select
          value={severity}
          onValueChange={(v) => setSeverity((v as (typeof BUG_SEVERITIES)[number]) ?? "")}
          placeholder="Any severity"
          className="filter-select"
        >
          {BUG_SEVERITIES.map((s) => (
            <Select.Item key={s} value={s}>
              {s}
            </Select.Item>
          ))}
        </Select>
        <Select
          value={priority}
          onValueChange={(v) => setPriority((v as (typeof BUG_PRIORITIES)[number]) ?? "")}
          placeholder="Any priority"
          className="filter-select"
        >
          {BUG_PRIORITIES.map((p) => (
            <Select.Item key={p} value={p}>
              {p}
            </Select.Item>
          ))}
        </Select>
        <Select value={productId} onValueChange={(v) => setProductId(v ?? "")} placeholder="Any product"
          className="filter-select"
          renderValue={(id) =>
            productsQ.state.status === "ready"
              ? productsQ.state.data.find((p) => p.id === id)?.name ?? id
              : id
          }
        >
          {productsQ.state.status === "ready" &&
            productsQ.state.data.map((p) => (
              <Select.Item key={p.id} value={p.id}>
                {p.name}
              </Select.Item>
            ))}
        </Select>
      </FilterBar>
      {pickerOpen ? (
        <Card className="column-picker-menu">
          <Cluster gap={3}>
            {ALL_BUG_COLUMNS.map((col) => (
              <Checkbox
                key={col.key}
                checked={columns.includes(col.key)}
                disabled={col.key === "summary"}
                onCheckedChange={() => toggleColumn(col.key)}
                label={col.label}
              />
            ))}
          </Cluster>
        </Card>
      ) : null}

      {/* No sort bar. Sorting lives on the table headers, where the thing
          being sorted is the thing you click -- a separate control naming a
          column you then have to find is an extra hop and can drift out of
          step with which columns are even visible. */}

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
        {(bugs: Bug[], refreshing: boolean) => (
          <>
            <BugResultsTable
              bugs={bugs}
              columns={columns}
              loading={refreshing}
              sort={{ key: SORT_COLUMN[sortBy] ?? "updated", direction: sortDirection === 1 ? "asc" : "desc" }}
              onSortChange={(next) => {
                setSortBy(COLUMN_SORT[next.key] ?? "updated_at");
                setSortDirection(next.direction === "asc" ? 1 : -1);
                setOffset(0);
              }}
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
