import { useState } from "react";
import {
  listProducts,
  reportByAssignee,
  reportByComponent,
  reportSummary,
  reportTimeToResolve,
  reportTrend,
} from "../api";
import { AsyncSection } from "../components/StateViews";
import { useAsync } from "../components/rpc";

function CountBars({ counts }: { counts: Record<string, number> }) {
  const entries = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  const max = Math.max(1, ...entries.map(([, v]) => v));
  if (entries.length === 0) return <p className="state-hint small">Nothing open.</p>;
  return (
    <ul className="bar-chart">
      {entries.map(([label, value]) => (
        <li key={label}>
          <span className="bar-label">{label}</span>
          <span className="bar-track">
            <span className="bar-fill" style={{ width: `${(value / max) * 100}%` }} />
          </span>
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
          <div className="stat-row">
            <div className="stat">
              <span className="stat-value">{summary.total}</span>
              <span className="stat-label">Total</span>
            </div>
            <div className="stat">
              <span className="stat-value">{summary.open}</span>
              <span className="stat-label">Open</span>
            </div>
          </div>
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
      emptyTitle="No open bugs in any component."
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
      emptyTitle="No open bugs assigned to anyone."
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
      emptyTitle="No created or resolved bugs in this window."
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
            <p className="state-hint small">No bugs resolved in this window.</p>
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
      <h1>Reports</h1>
      <div className="filter-bar">
        <select value={productId} onChange={(e) => setProductId(e.target.value)}>
          <option value="">All products</option>
          {productsQ.state.status === "ready" &&
            productsQ.state.data.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
        </select>
        <label>
          Window (days)
          <input
            type="number"
            min={1}
            max={365}
            value={days}
            onChange={(e) => setDays(Math.min(365, Math.max(1, Number(e.target.value) || 30)))}
          />
        </label>
      </div>
      <SummarySection productId={productId} />
      <ByComponentSection productId={productId} productNames={productNames} />
      <ByAssigneeSection productId={productId} />
      <TrendSection productId={productId} days={days} />
      <TimeToResolveSection productId={productId} days={days} />
    </div>
  );
}
