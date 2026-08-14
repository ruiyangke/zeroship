import { useState } from "react";
import {
  listProducts,
  reportByAssignee,
  reportByComponent,
  reportSummary,
  reportTimeToResolve,
  reportTrend,
} from "../api";
import { Cluster, Field, NumberField, PageHeader, Progress, Select, StatCard } from "@zeroship/ui";
import { AsyncSection } from "../components/StateViews";
import { useAsync } from "../components/rpc";

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

function SummarySection({ productId }: { productId: string }) {
  const { state, reload } = useAsync(() => reportSummary({ productId: productId || undefined }), [productId]);
  return (
    <AsyncSection state={state} onRetry={reload} loadingLabel="Loading summary...">
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
  const { state, reload } = useAsync(
    () => reportByComponent({ productId: productId || undefined }),
    [productId],
  );
  return (
    <AsyncSection
      state={state}
      onRetry={reload}
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
  const { state, reload } = useAsync(
    () => reportByAssignee({ productId: productId || undefined }),
    [productId],
  );
  return (
    <AsyncSection
      state={state}
      onRetry={reload}
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
  const { state, reload } = useAsync(
    () => reportTrend({ productId: productId || undefined, days }),
    [productId, days],
  );
  return (
    <AsyncSection
      state={state}
      onRetry={reload}
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
  const { state, reload } = useAsync(
    () => reportTimeToResolve({ productId: productId || undefined, days }),
    [productId, days],
  );
  return (
    <AsyncSection state={state} onRetry={reload} loadingLabel="Loading resolution time...">
      {(data) =>
        data.resolvedCount === 0 ? (
          <section className="report-section">
            <h2>Time to resolve</h2>
            <p className="state-hint small">No issues resolved in this window.</p>
          </section>
        ) : (
          <section className="report-section">
            <h2>Time to resolve</h2>
            <div className="stat-row">
              <div className="stat">
                <span className="stat-value">{data.resolvedCount}</span>
                <span className="stat-label">Resolved</span>
              </div>
              <div className="stat">
                <span className="stat-value">{humanMs(data.averageMs ?? 0)}</span>
                <span className="stat-label">Average</span>
              </div>
              <div className="stat">
                <span className="stat-value">{humanMs(data.medianMs ?? 0)}</span>
                <span className="stat-label">Median</span>
              </div>
            </div>
          </section>
        )
      }
    </AsyncSection>
  );
}

export function ReportsPage() {
  const [productId, setProductId] = useState("");
  const [days, setDays] = useState(30);
  const productsQ = useAsync(() => listProducts({}), []);
  const productNames =
    productsQ.state.status === "ready"
      ? Object.fromEntries(productsQ.state.data.map((p) => [p.id, p.name]))
      : {};

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
      <ByComponentSection productId={productId} productNames={productNames} />
      <ByAssigneeSection productId={productId} />
      <TrendSection productId={productId} days={days} />
      <TimeToResolveSection productId={productId} days={days} />
    </div>
  );
}
