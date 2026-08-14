import { useState } from "react";
import { Cluster, Field, NumberField, PageHeader, Progress, Select, StatCard } from "@zeroship/ui";
import {
  useProducts,
  useReportByAssignee,
  useReportByComponent,
  useReportSummary,
  useReportTimeToResolve,
  useReportTrend,
} from "../lib/queries";
import { AsyncSection } from "../components/StateViews";

function CountBars({ counts }: { counts: Record<string, number> }) {
  const entries = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  const max = Math.max(1, ...entries.map(([, v]) => v));
  if (entries.length === 0) return <p className="state-hint small">Nothing open.</p>;
  return (
    <ul className="bar-chart">
      {/* A real Progress rather than two nested spans faking a track and a
          fill with an inline width percentage. It carries the value, max and
          role, so the bar is readable by something other than an eye. */}
      {entries.map(([label, value]) => (
        <li key={label}>
          <span className="bar-label">{label}</span>
          <Progress value={value} max={max} aria-label={`${label}: ${value}`} />
          <span className="bar-value">{value}</span>
        </li>
      ))}
    </ul>
  );
}

/**
 * The filter's "all products" value, as the cache spells it.
 *
 * The picker holds "" for all products and every hook keys on `string | null`,
 * so the empty string is normalised HERE rather than in five places. Two
 * spellings of the same filter would be two cache entries holding the same
 * answer, refetched independently.
 */
function productKey(productId: string): string | null {
  return productId || null;
}

function SummarySection({ productId }: { productId: string }) {
  const summaryQ = useReportSummary(productKey(productId));
  return (
    <AsyncSection query={summaryQ} loadingLabel="Loading summary...">
      {(summary) => (
        <section className="report-section">
          <h2>Summary</h2>
          {/* StatCards rather than hand-rolled stat divs. Three of them: the
              closed count was derivable from the other two and a reader should
              not have to do the subtraction to answer "how are we doing". */}
          <Cluster gap={3}>
            <StatCard label="Total" value={summary.total} />
            <StatCard label="Open" value={summary.open} />
            <StatCard label="Closed" value={summary.total - summary.open} />
          </Cluster>
          <div className="report-grid">
            <div>
              <h3>By status</h3>
              <CountBars counts={summary.byStatus} />
            </div>
            {/* Kind sits BESIDE severity, not folded into it. While a feature
                request was `severity: enhancement`, "by severity" answered two
                questions at once and neither cleanly -- the enhancement bar
                counted things that have no severity, and every other bar was a
                defect count wearing a general label. `reports.summary` split
                them; this report kept reading three of the four breakdowns, so
                it was silently wrong by omission about what the open work IS. */}
            <div>
              <h3>By kind</h3>
              <CountBars counts={summary.byKind} />
            </div>
            <div>
              <h3>By severity</h3>
              <CountBars counts={summary.bySeverity} />
            </div>
            <div>
              <h3>By priority</h3>
              <CountBars counts={summary.byPriority} />
            </div>
          </div>
        </section>
      )}
    </AsyncSection>
  );
}

type ComponentRow = { component?: { id: string; name: string; productId: string } | null };

/**
 * Name a component row so it can be told apart from the others on screen.
 *
 * Component names are only unique WITHIN a product -- "Core" and "Parser" are
 * what every product calls its first two -- so an unqualified name turns the
 * report into a column of identical labels. Measured against the dev database:
 * 54 rows, every one of them reading "Core" or "Parser", each with its own
 * count. The numbers were right and the table was unreadable.
 *
 * The product is added only when the name is actually ambiguous in THIS
 * result set. Qualifying unconditionally would push "Bugzilla / Core" into
 * every row of a single-product report, where the product is already fixed by
 * the filter above and repeating it is noise.
 */
function componentLabel(
  component: ComponentRow["component"],
  rows: readonly ComponentRow[],
  productNames: Record<string, string>,
): string {
  if (!component) return "(deleted component)";
  const sameName = rows.filter((r) => r.component?.name === component.name);
  if (sameName.length < 2) return component.name;
  // Falls back to the id rather than dropping the qualifier: a product the
  // caller cannot see is still a distinct product, and an unqualified row
  // would collide with the one above it again.
  const product = productNames[component.productId] ?? component.productId;
  return `${product} / ${component.name}`;
}

function ByComponentSection({
  productId,
  productNames,
}: {
  productId: string;
  productNames: Record<string, string>;
}) {
  const byComponentQ = useReportByComponent(productKey(productId));
  return (
    <AsyncSection
      query={byComponentQ}
      loadingLabel="Loading component report..."
      isEmpty={(data) => data.length === 0}
      emptyTitle="No open issues in any component."
    >
      {(rows) => (
        <section className="report-section">
          <h2>By component</h2>
          <table className="report-table">
            <thead>
              <tr>
                <th>Component</th>
                <th>Open</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row, i) => (
                <tr key={row.component?.id ?? i}>
                  <td>{componentLabel(row.component, rows, productNames)}</td>
                  <td>{row.count}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
    </AsyncSection>
  );
}

function ByAssigneeSection({ productId }: { productId: string }) {
  const byAssigneeQ = useReportByAssignee(productKey(productId));
  return (
    <AsyncSection
      query={byAssigneeQ}
      loadingLabel="Loading assignee report..."
      isEmpty={(data) => data.length === 0}
      emptyTitle="No open issues assigned to anyone."
    >
      {(rows) => (
        <section className="report-section">
          <h2>By assignee</h2>
          <table className="report-table">
            <thead>
              <tr>
                <th>Assignee</th>
                <th>Open</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row, i) => (
                <tr key={row.assignee?.id ?? i}>
                  <td>{row.assignee?.name ?? "Unassigned"}</td>
                  <td>{row.count}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
    </AsyncSection>
  );
}

function TrendSection({ productId, days }: { productId: string; days: number }) {
  const trendQ = useReportTrend(productKey(productId), days);
  return (
    <AsyncSection
      query={trendQ}
      loadingLabel="Loading trend..."
      isEmpty={(data) => data.every((d) => d.created === 0 && d.resolved === 0)}
      emptyTitle="No created or resolved issues in this window."
    >
      {(rows) => {
        const max = Math.max(1, ...rows.flatMap((r) => [r.created, r.resolved]));
        return (
          <section className="report-section">
            <h2>Trend ({days}d)</h2>
            <div className="trend-chart">
              {rows.map((row) => (
                <div key={row.date} className="trend-day" title={row.date}>
                  <div className="trend-bars">
                    <span
                      className="trend-bar created"
                      style={{ height: `${(row.created / max) * 100}%` }}
                    />
                    <span
                      className="trend-bar resolved"
                      style={{ height: `${(row.resolved / max) * 100}%` }}
                    />
                  </div>
                </div>
              ))}
            </div>
            <div className="trend-legend">
              <span className="chip created">created</span>
              <span className="chip resolved">resolved</span>
            </div>
          </section>
        );
      }}
    </AsyncSection>
  );
}

function humanMs(ms: number): string {
  const hours = ms / 3_600_000;
  if (hours < 48) return `${hours.toFixed(1)}h`;
  return `${(hours / 24).toFixed(1)}d`;
}

function TimeToResolveSection({ productId, days }: { productId: string; days: number }) {
  const timeToResolveQ = useReportTimeToResolve(productKey(productId), days);
  return (
    <AsyncSection query={timeToResolveQ} loadingLabel="Loading resolution time...">
      {(data) =>
        data.resolvedCount === 0 ? (
          <section className="report-section">
            <h2>Time to resolve</h2>
            <p className="state-hint small">No issues resolved in this window.</p>
          </section>
        ) : (
          <section className="report-section">
            <h2>Time to resolve ({days}d)</h2>
            {/* StatCards, like the summary above. These were the last three
                hand-rolled `.stat` divs in the app: same three numbers, same
                row, drawn by a second private copy of the treatment the
                summary had already replaced. The heading also names its
                window, because these three are the only numbers on the page
                that are NOT all-time and nothing said so. */}
            <Cluster gap={3}>
              <StatCard label="Resolved" value={data.resolvedCount} />
              <StatCard label="Average" value={humanMs(data.averageMs ?? 0)} />
              <StatCard label="Median" value={humanMs(data.medianMs ?? 0)} />
            </Cluster>
          </section>
        )
      }
    </AsyncSection>
  );
}

export function ReportsPage() {
  const [productId, setProductId] = useState("");
  const [days, setDays] = useState(30);
  const productsQ = useProducts({});
  const products = productsQ.data ?? [];
  const productNames = Object.fromEntries(products.map((p) => [p.id, p.name]));

  return (
    <div className="page reports-page">
      <PageHeader>
        <PageHeader.Title>Reports</PageHeader.Title>
      </PageHeader>
      {/* Both filters read the same way. The product picker carried only an
          aria-label while "Window (days)" beside it showed a visible one, so
          the bar had one labelled control and one bare box whose meaning you
          inferred from its placeholder. */}
      <div className="filter-bar report-filters">
        <Field className="report-filter report-filter--product">
          <Field.Label>Product</Field.Label>
          <Select
          value={productId}
          onValueChange={(next) => setProductId(next ?? "")}
          placeholder="All products"
          aria-label="Product"
          className="filter-select"
          renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}
        >
          {products.map((p) => (
            <Select.Item key={p.id} value={p.id}>
              {p.name}
            </Select.Item>
          ))}
          </Select>
        </Field>
        <Field className="report-filter report-filter--window">
          <Field.Label>Window (days)</Field.Label>
          <NumberField
            min={1}
            max={365}
            value={days}
            onValueChange={(next) => setDays(Math.min(365, Math.max(1, next ?? 30)))}
          />
        </Field>
      </div>
      <SummarySection productId={productId} />
      {/* Paired across the page rather than stacked down it. Every one of these
          four is a narrow thing -- a two-column table, or three numbers -- and
          full-width bands left most of a 1440px page empty while pushing the
          later reports two screens down. The sidebar is gone; the page has the
          width to show them together, and comparing "which component" against
          "which person" side by side is the whole reason both exist. */}
      <div className="report-pair">
        <ByComponentSection productId={productId} productNames={productNames} />
        <ByAssigneeSection productId={productId} />
      </div>
      <div className="report-pair">
        <TrendSection productId={productId} days={days} />
        <TimeToResolveSection productId={productId} days={days} />
      </div>
    </div>
  );
}
