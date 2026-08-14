import { useState } from "react";
import { useClearQuery, useNumericQueryParam, useQueryParam } from "../lib/query-state";
import { Button, Card, Checkbox, Cluster, Dialog, FilterBar, PageHeader, Select } from "@zeroship/ui";
import {
  ALL_ISSUE_COLUMNS,
  IssueResultsTable,
  type IssueColumnKey,
} from "../components/IssueResultsTable";
import { useIssueSearch, useProducts } from "../lib/queries";
import { FieldBuilder } from "../components/search/SearchBuilder";
import { AsyncSection } from "../components/StateViews";
import { isVisitor, useSession } from "../components/session";
import type { Issue } from "../components/types";
import { ISSUE_STATUSES, type IssueStatus } from "../lib/workflow";
import { ISSUE_KINDS, ISSUE_PRIORITIES, ISSUE_SEVERITIES, parseQuickSearch } from "../lib/quicksearch";

/**
 * The parser throws on malformed input ("@" with no handle). A half-typed
 * query is the normal state of a search box, so a throw there would blank the
 * list mid-keystroke; an unparseable string is simply treated as text.
 */
function parseQuickSearchSafely(input: string) {
  if (!input.trim()) return [];
  try {
    return parseQuickSearch(input);
  } catch {
    return [{ field: "text" as const, value: input }];
  }
}

// `kind` is DEFAULT, not opt-in like `reporter`. It was briefly opt-in, and a
// screenshot of the result settled it: "Support dark mode in the viewer" and
// "Crash on empty input" rendered identically, because the only classification
// on screen was a severity badge and severity no longer says what a record is.
// A field that exists so two things can be told apart has to be visible in the
// view where you are telling them apart.
const DEFAULT_COLUMNS: IssueColumnKey[] = [
  "id",
  "status",
  "resolution",
  "kind",
  "severity",
  "priority",
  "product",
  "summary",
  "assignee",
  "updated",
];

const COLUMNS_STORAGE_KEY = "issue-tracker:list-columns";
const PAGE_SIZE = 25;

function loadColumns(): IssueColumnKey[] {
  try {
    const raw = window.localStorage.getItem(COLUMNS_STORAGE_KEY);
    if (!raw) return DEFAULT_COLUMNS;
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) return DEFAULT_COLUMNS;
    const valid = parsed.filter((c): c is IssueColumnKey =>
      ALL_ISSUE_COLUMNS.some((col) => col.key === c),
    );
    return valid.length > 0 ? valid : DEFAULT_COLUMNS;
  } catch {
    return DEFAULT_COLUMNS;
  }
}

const SORTABLE = ["created_at", "updated_at", "priority", "severity", "status"] as const;
type SortBy = (typeof SORTABLE)[number];

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

export function IssueListPage() {
  // The query lives in the URL, so a narrowed list is a link you can send.
  // Typing replaces the entry (one per keystroke would bury the Back button);
  // the discrete choices push, so Back undoes them one at a time.
  const [text, setText] = useQueryParam("q", "", { replace: true });
  const [statusParam, setStatus] = useQueryParam("status");
  const [kindParam, setKind] = useQueryParam("kind");
  const [severityParam, setSeverity] = useQueryParam("severity");
  const [priorityParam, setPriority] = useQueryParam("priority");
  const [productId, setProductId] = useQueryParam("product");
  const [sortByParam, setSortByParam] = useQueryParam("sort", "updated_at");
  const [sortDirParam, setSortDirParam] = useQueryParam("dir", "-1");
  const [offset, setOffset] = useNumericQueryParam("offset", 0);

  // The URL is a string typed by anyone; the rest of this page expects the
  // unions. An unknown value reads as "no filter" rather than being passed to
  // the server, so a hand-edited ?status=nonsense narrows nothing instead of
  // erroring.
  const status = (ISSUE_STATUSES as readonly string[]).includes(statusParam)
    ? (statusParam as IssueStatus)
    : "";
  const kind = (ISSUE_KINDS as readonly string[]).includes(kindParam)
    ? (kindParam as (typeof ISSUE_KINDS)[number])
    : "";
  const severity = (ISSUE_SEVERITIES as readonly string[]).includes(severityParam)
    ? (severityParam as (typeof ISSUE_SEVERITIES)[number])
    : "";
  const priority = (ISSUE_PRIORITIES as readonly string[]).includes(priorityParam)
    ? (priorityParam as (typeof ISSUE_PRIORITIES)[number])
    : "";
  const sortBy = (SORTABLE as readonly string[]).includes(sortByParam)
    ? (sortByParam as SortBy)
    : "updated_at";
  const sortDirection: 1 | -1 = sortDirParam === "1" ? 1 : -1;
  const setSortBy = (next: SortBy) => setSortByParam(next);
  const setSortDirection = (next: 1 | -1) => setSortDirParam(String(next));
  const [columns, setColumns] = useState<IssueColumnKey[]>(loadColumns);
  const [pickerOpen, setPickerOpen] = useState(false);
  // Results from the advanced builder REPLACE the filtered list while they
  // are set. There is no second results page any more: the builder is a modal
  // over this one, and a search that lands somewhere else is a search whose
  // result you then have to go and find.
  const [advanced, setAdvanced] = useState<Issue[] | null>(null);
  const [builderOpen, setBuilderOpen] = useState(false);

  // The same entry the filing form and the issue page read. `products.list`
  // was measured at FOUR requests on one load of this page; keyed, it is one.
  const productsQ = useProducts();
  const products = productsQ.data ?? [];

  // The list itself is public -- `issues.search` and `products.list` are both
  // `auth: "anon", publiclyAccessible: true` -- so this page does NOT become a
  // sign-in wall. It is asked only about the one control that is not public:
  // the advanced builder runs `search.query` and reads `savedSearches.list`,
  // both `auth: "user"`. Same shape as the issue page (`signedOut` from
  // `users.me` + `isUnauthenticated`), so there is one way to ask this
  // question in the app rather than two.
  const session = useSession();
  const signedOut = isVisitor(session);
  // NOT `!signedIn`. While the answer is in flight both are false, which is
  // the point: the page commits to neither reading until the server has said.
  const identityUnknown = session.status !== "in";

  /**
   * The search box speaks Bugzilla QuickSearch.
   *
   * "P1 @alice comp:parser" used to need its own box on its own page. That
   * page is gone, and rather than bury the shorthand in the modal it is
   * parsed HERE: recognised tokens become the same filters the dropdowns set,
   * so they show up as removable chips and the query stays one call to
   * issues.search -- which is anonymous, where search.quick is not.
   *
   * Only the tokens that map to a filter this page already has. An assignee,
   * product or component token names something by handle or name and would
   * need resolving to an id first, so those stay as free text rather than
   * being silently dropped.
   */
  const parsed = parseQuickSearchSafely(text);
  const tokenStatus = parsed.find((c) => c.field === "status")?.value as IssueStatus | undefined;
  const tokenKind = parsed.find((c) => c.field === "kind")?.value as
    | (typeof ISSUE_KINDS)[number]
    | undefined;
  const tokenSeverity = parsed.find((c) => c.field === "severity")?.value as
    | (typeof ISSUE_SEVERITIES)[number]
    | undefined;
  const tokenPriority = parsed.find((c) => c.field === "priority")?.value as
    | (typeof ISSUE_PRIORITIES)[number]
    | undefined;
  const freeText = parsed
    .filter((c) => c.field === "text")
    .map((c) => c.value)
    .join(" ");

  /**
   * The filters ARE the key, so changing one is a different question and the
   * cache answers it without this page arranging a refetch.
   *
   * The rows already on screen stay while the next answer loads --
   * `useIssueSearch` sets `placeholderData` for exactly that. It is worth
   * saying here too because it is the behaviour this page earned the hard way:
   * dropping to a loading state on every refetch "blanked the section and
   * jumped the layout" on each filter change, sort and page.
   */
  const issuesQ = useIssueSearch({
    text: freeText.trim() || undefined,
    status: status || tokenStatus || undefined,
    kind: kind || tokenKind || undefined,
    severity: severity || tokenSeverity || undefined,
    priority: priority || tokenPriority || undefined,
    productId: productId || undefined,
    sortBy,
    sortDirection,
    limit: PAGE_SIZE,
    offset,
  });


  /**
   * Columns whose CONTENT is authenticated, dropped for a visitor.
   *
   * `users.resolve` is `auth: "user"` while `issues.search` is not, so a
   * signed-out reader gets the rows but no names, and Assignee and Reporter
   * render a full column of "--". `useIssueLookups` documents that fallback as
   * intended degradation and it is -- for a cell. A whole column of it is not
   * degradation, it is a column that answers nothing, and it was still taking
   * width away from Summary.
   *
   * Dropped from the PICKER too, not just the table. Offering a checkbox that
   * turns on a column of dashes is the same false affordance as the "New issue"
   * button this page used to show a visitor.
   */
  const identityColumns: IssueColumnKey[] = ["assignee", "reporter"];
  const availableColumns = identityUnknown
    ? ALL_ISSUE_COLUMNS.filter((col) => !identityColumns.includes(col.key))
    : ALL_ISSUE_COLUMNS;
  const visibleColumns = identityUnknown
    ? columns.filter((key) => !identityColumns.includes(key))
    : columns;

  const toggleColumn = (key: IssueColumnKey) => {
    setColumns((prev) => {
      const next = prev.includes(key) ? prev.filter((c) => c !== key) : [...prev, key];
      const ordered = ALL_ISSUE_COLUMNS.map((c) => c.key).filter((c) => next.includes(c));
      window.localStorage.setItem(COLUMNS_STORAGE_KEY, JSON.stringify(ordered));
      return ordered;
    });
  };

  // One write, not six. See useClearQuery.
  const resetFilters = useClearQuery();

  // Applying resets to the first page. Staying on page 4 of the old result set
  // while the filter changes shows an empty table for a query that matched
  // plenty, which reads as "no results" rather than "you are past the end".
  const applyFilters = () => {
    setOffset(0);
    // A refetch, not a reload of a hand-rolled state machine. The filters
    // themselves are already in the key, so this button is the "I typed
    // something and want it now" affordance rather than the mechanism.
    void issuesQ.refetch();
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
    // Tokens typed into the box are shown as chips too, marked so it is clear
    // they came from the query rather than a dropdown, and removed by editing
    // the text since that is where they live.
    tokenStatus && !status
      ? { id: "t-status", label: `Status: ${tokenStatus} (typed)`, onRemove: () => setText(freeText) }
      : null,
    tokenKind && !kind
      ? { id: "t-kind", label: `Kind: ${tokenKind} (typed)`, onRemove: () => setText(freeText) }
      : null,
    tokenSeverity && !severity
      ? {
          id: "t-severity",
          label: `Severity: ${tokenSeverity} (typed)`,
          onRemove: () => setText(freeText),
        }
      : null,
    tokenPriority && !priority
      ? {
          id: "t-priority",
          label: `Priority: ${tokenPriority} (typed)`,
          onRemove: () => setText(freeText),
        }
      : null,
    status ? { id: "status", label: `Status: ${status}`, onRemove: () => setStatus("") } : null,
    kind ? { id: "kind", label: `Kind: ${kind}`, onRemove: () => setKind("") } : null,
    severity
      ? { id: "severity", label: `Severity: ${severity}`, onRemove: () => setSeverity("") }
      : null,
    priority
      ? { id: "priority", label: `Priority: ${priority}`, onRemove: () => setPriority("") }
      : null,
    productId
      ? {
          id: "product",
          label: `Product: ${products.find((p) => p.id === productId)?.name ?? productId}`,
          onRemove: () => setProductId(""),
        }
      : null,
  ].filter((chip): chip is NonNullable<typeof chip> => chip !== null);

  return (
    <div className="page issue-list-page">
      {/* No page-level "New issue": the shell header carries it on every
          page, and two of them side by side was the first thing the
          screenshot showed. */}
      {/* Compound parts, not a `title` prop -- PageHeader is a container and
          passing title rendered nothing at all, which reads as a missing
          heading rather than a wrong API. */}
      <PageHeader>
        <PageHeader.Title>Issues</PageHeader.Title>
      </PageHeader>

      {/* FilterBar owns the row: a real search Input, the applied filters as
          removable chips, and the pickers in the controls slot. The chips are
          the point -- four always-visible dropdowns showed which filters
          EXIST, never which are ON, so a narrowed list looked identical to an
          empty one. */}
      <FilterBar
        search={text}
        onSearchChange={setText}
        searchPlaceholder="Search, or type P1 CONFIRMED major..."
        activeFilters={activeFilters}
        onClearFilters={activeFilters.length > 0 ? resetFilters : undefined}
        actions={
          <Cluster gap={2} align="center">
            <Button variant="gray" size="small" onClick={applyFilters}>
              Apply
            </Button>
            {/* The builder needs an identity and the filters do not. Signed
                out this opened a dialog whose Run search 401s and whose saved
                searches never load, which is a fully operable-looking control
                that cannot work -- exactly what the issue page stopped doing
                with its composer. Filtering, sorting, paging and the column
                picker stay: they are all served by the anonymous
                `issues.search`. */}
            {signedOut ? null : (
              <Button variant="gray" size="small" onClick={() => setBuilderOpen(true)}>
                Advanced...
              </Button>
            )}
            {/* The menu lives WITH its button, in a positioned wrapper.
                It used to be a sibling of the whole FilterBar, so
                `position: absolute; top: 110%` resolved against the nearest
                positioned ancestor -- the page -- and 110% of a tall block put
                the menu at y=1692 in a 900px viewport. It opened every time
                and was 792px below the fold, so the button read as dead. */}
            <div className="column-picker">
              <Button
                variant="plain"
                size="small"
                onClick={() => setPickerOpen((open) => !open)}
                aria-expanded={pickerOpen}
              >
                Columns
              </Button>
              {pickerOpen ? (
                <Card className="column-picker-menu">
                  <Cluster gap={3}>
                    {availableColumns.map((col) => (
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
            </div>
          </Cluster>
        }
      >
        <Select value={status} onValueChange={(v) => setStatus((v as IssueStatus) ?? "")} placeholder="Any status" className="filter-select">
          {ISSUE_STATUSES.map((s) => (
            <Select.Item key={s} value={s}>
              {s}
            </Select.Item>
          ))}
        </Select>
        {/* Beside severity, and before it: kind decides what the severity
            beside it is even describing. */}
        <Select
          value={kind}
          onValueChange={(v) => setKind((v as (typeof ISSUE_KINDS)[number]) ?? "")}
          placeholder="Any kind"
          className="filter-select"
        >
          {ISSUE_KINDS.map((k) => (
            <Select.Item key={k} value={k}>
              {k}
            </Select.Item>
          ))}
        </Select>
        <Select
          value={severity}
          onValueChange={(v) => setSeverity((v as (typeof ISSUE_SEVERITIES)[number]) ?? "")}
          placeholder="Any severity"
          className="filter-select"
        >
          {ISSUE_SEVERITIES.map((s) => (
            <Select.Item key={s} value={s}>
              {s}
            </Select.Item>
          ))}
        </Select>
        <Select
          value={priority}
          onValueChange={(v) => setPriority((v as (typeof ISSUE_PRIORITIES)[number]) ?? "")}
          placeholder="Any priority"
          className="filter-select"
        >
          {ISSUE_PRIORITIES.map((p) => (
            <Select.Item key={p} value={p}>
              {p}
            </Select.Item>
          ))}
        </Select>
        <Select value={productId} onValueChange={(v) => setProductId(v ?? "")} placeholder="Any product"
          className="filter-select"
          renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}
        >
          {products.map((p) => (
            <Select.Item key={p.id} value={p.id}>
              {p.name}
            </Select.Item>
          ))}
        </Select>
      </FilterBar>
      {/* No sort bar. Sorting lives on the table headers, where the thing
          being sorted is the thing you click -- a separate control naming a
          column you then have to find is an extra hop and can drift out of
          step with which columns are even visible. */}

      {/* The builder, as a modal over this page. */}
      <Dialog open={builderOpen} onOpenChange={setBuilderOpen}>
        <Dialog.Portal>
          <Dialog.Backdrop />
          <Dialog.Popup className="search-builder-popup">
            <Dialog.Header>
              <Dialog.Title>Advanced search</Dialog.Title>
              <Dialog.Description>
                Build a structured query, or reuse one you saved.
              </Dialog.Description>
            </Dialog.Header>
            <Dialog.Body>
              <FieldBuilder
                onResults={(rows) => {
                  setAdvanced(rows);
                  setBuilderOpen(false);
                }}
              />
              {/* No second SavedSearchesPanel here: FieldBuilder renders one
                  itself, with the CURRENT query bound to it, so adding
                  another showed the section twice and the lower copy could
                  only ever save an empty search. */}
            </Dialog.Body>
            <Dialog.Footer>
              <Dialog.Close>Close</Dialog.Close>
            </Dialog.Footer>
          </Dialog.Popup>
        </Dialog.Portal>
      </Dialog>

      {advanced ? (
        <>
          {/* Says what you are looking at and how to leave it. A results set
              that silently replaced the list would be indistinguishable from
              a filter that happened to match those rows. */}
          <Cluster gap={2} align="center">
            <span className="state-hint small">
              Showing {advanced.length} result{advanced.length === 1 ? "" : "s"} from advanced
              search
            </span>
            <Button variant="plain" size="small" onClick={() => setAdvanced(null)}>
              Back to filters
            </Button>
          </Cluster>
          <IssueResultsTable issues={advanced} columns={visibleColumns} caption="Advanced search results" />
        </>
      ) : (
      <AsyncSection
        query={issuesQ}
        loadingLabel="Loading issues..."
        /* The table marks itself as loading rather than being replaced by a
           spinner. Swapping the whole surface out discards the headers and
           the page height, so the layout jumped when rows arrived -- the
           first load was the one case still doing it, because `loading` on
           the table below only applies once there is data to keep. */
        renderLoading={() => (
          <IssueResultsTable
            issues={[]}
            columns={visibleColumns}
            loading
            sort={{
              key: SORT_COLUMN[sortBy] ?? "updated",
              direction: sortDirection === 1 ? "asc" : "desc",
            }}
            onSortChange={() => {}}
          />
        )}
        isEmpty={(data) => data.length === 0}
        emptyTitle={
          text || status || kind || severity || priority || productId
            ? "No issues match these filters."
            : "No issues visible."
        }
        emptyHint={
          text || status || kind || severity || priority || productId
            ? "Try widening the filters."
            : "Either nothing has been filed yet, or there is no signed-in identity with visibility into any product."
        }
      >
        {(issues: Issue[], refreshing: boolean) => (
          <>
            <IssueResultsTable
              issues={issues}
              columns={visibleColumns}
              loading={refreshing}
              sort={{ key: SORT_COLUMN[sortBy] ?? "updated", direction: sortDirection === 1 ? "asc" : "desc" }}
              onSortChange={(next) => {
                setSortBy(COLUMN_SORT[next.key] ?? "updated_at");
                setSortDirection(next.direction === "asc" ? 1 : -1);
                setOffset(0);
              }}
            />
            <div className="pager">
              <Button variant="gray" size="small"
                disabled={offset === 0}
                onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
              >
                Previous
              </Button>
              <span>
                {offset + 1}-{offset + issues.length}
              </span>
              <Button variant="gray" size="small"
                disabled={issues.length < PAGE_SIZE}
                onClick={() => setOffset(offset + PAGE_SIZE)}
              >
                Next
              </Button>
            </div>
          </>
        )}
      </AsyncSection>
      )}
    </div>
  );
}
