import { useState, type ReactNode } from "react";
import { Badge, Field, NumberField, PageHeader, Progress, Select, StatCard } from "@zeroship/ui";
import {
  useProducts,
  useReportByAssignee,
  useReportByComponent,
  useReportSummary,
  useReportTimeToResolve,
  useReportTrend,
} from "../lib/queries";
import { AsyncSection } from "../components/StateViews";
import { FilterControl, Hint, InlineForm, Page, SectionHeading } from "../components/AppPrimitives";

function ReportSection({ title, children }: { title: ReactNode; children: ReactNode }) {
  return (
    <section className="report-section mb-5 rounded-lg border border-line bg-surface p-4">
      <SectionHeading>{title}</SectionHeading>
      {children}
    </section>
  );
}

function ReportPair({ children }: { children: ReactNode }) {
  return (
    <div className="grid grid-cols-1 items-start gap-5 min-[1101px]:grid-cols-2 [&>*]:min-w-0">
      {children}
    </div>
  );
}

function CountBar({ label, value, max }: { label: string; value: number; max: number }) {
  return (
    <li className="grid grid-cols-[90px_1fr_32px] items-center gap-2 text-sm">
      <span className="text-ink-secondary capitalize">{label}</span>
      <Progress value={value} max={max} aria-label={`${label}: ${value}`} />
      <span className="text-right text-ink-secondary tabular-nums">{value}</span>
    </li>
  );
}

function CountBars({ counts }: { counts: Record<string, number> }) {
  const entries = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  const max = Math.max(1, ...entries.map(([, v]) => v));
  if (entries.length === 0) return <Hint>Nothing open.</Hint>;
  return (
    <ul className="m-0 flex list-none flex-col gap-2 p-0">
      {/* A real Progress rather than two nested spans faking a track and a
          fill with an inline width percentage. It carries the value, max and
          role, so the bar is readable by something other than an eye. */}
      {entries.map(([label, value]) => (
        <CountBar key={label} label={label} value={value} max={max} />
      ))}
    </ul>
  );
}

function CountBreakdown({ title, counts }: { title: string; counts: Record<string, number> }) {
  return (
    <div>
      <SectionHeading level={3}>{title}</SectionHeading>
      <CountBars counts={counts} />
    </div>
  );
}

type CountReportRow = {
  key: string | number;
  label: ReactNode;
  count: number;
};

function CountReportTable({
  labelHeading,
  rows,
}: {
  labelHeading: string;
  rows: readonly CountReportRow[];
}) {
  return (
    <table className="w-full border-collapse text-base">
      <thead>
        <tr>
          <th className="border-b border-line bg-surface-sunken px-2 py-2 text-left text-xs font-semibold uppercase tracking-[0.04em] text-ink-muted whitespace-nowrap">
            {labelHeading}
          </th>
          <th className="w-20 border-b border-line bg-surface-sunken px-2 py-2 text-right text-xs font-semibold uppercase tracking-[0.04em] text-ink-muted whitespace-nowrap tabular-nums">
            Open
          </th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row) => (
          <tr key={row.key} className="hover:bg-surface-sunken">
            <td className="border-b border-line px-2 py-2 text-left whitespace-nowrap">
              {row.label}
            </td>
            <td className="w-20 border-b border-line px-2 py-2 text-right whitespace-nowrap tabular-nums">
              {row.count}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
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
        <ReportSection title="Summary">
          {/* StatCards rather than hand-rolled stat divs. Three of them: the
              closed count was derivable from the other two and a reader should
              not have to do the subtraction to answer "how are we doing". */}
          <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-3">
            <StatCard label="Total" value={summary.total} />
            <StatCard label="Open" value={summary.open} />
            <StatCard label="Closed" value={summary.total - summary.open} />
          </div>
          <div className="mt-2 grid grid-cols-1 gap-5 min-[901px]:grid-cols-2 min-[1101px]:grid-cols-4">
            <CountBreakdown title="By status" counts={summary.byStatus} />
            {/* Kind sits BESIDE severity, not folded into it. While a feature
                request was `severity: enhancement`, "by severity" answered two
                questions at once and neither cleanly -- the enhancement bar
                counted things that have no severity, and every other bar was a
                defect count wearing a general label. `reports.summary` split
                them; this report kept reading three of the four breakdowns, so
                it was silently wrong by omission about what the open work IS. */}
            <CountBreakdown title="By kind" counts={summary.byKind} />
            <CountBreakdown title="By severity" counts={summary.bySeverity} />
            <CountBreakdown title="By priority" counts={summary.byPriority} />
          </div>
        </ReportSection>
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
        <ReportSection title="By component">
          <CountReportTable
            labelHeading="Component"
            rows={rows.map((row, index) => ({
              key: row.component?.id ?? index,
              label: componentLabel(row.component, rows, productNames),
              count: row.count,
            }))}
          />
        </ReportSection>
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
        <ReportSection title="By assignee">
          <CountReportTable
            labelHeading="Assignee"
            rows={rows.map((row, index) => ({
              key: row.assignee?.id ?? index,
              label: row.assignee?.name ?? "Unassigned",
              count: row.count,
            }))}
          />
        </ReportSection>
      )}
    </AsyncSection>
  );
}

type TrendKind = "created" | "resolved";
type TrendRow = { date: string; created: number; resolved: number };

function TrendBar({ value, max, kind }: { value: number; max: number; kind: TrendKind }) {
  return (
    <span
      className={`min-h-px flex-1 rounded-t-sm ${
        kind === "created" ? "bg-info" : "bg-accent-strong"
      }`}
      style={{ height: `${(value / max) * 100}%` }}
    />
  );
}

function TrendDay({ row, max }: { row: TrendRow; max: number }) {
  return (
    <div className="flex h-full flex-1 items-end" title={row.date}>
      <div className="flex h-full w-full items-end gap-[1px]">
        <TrendBar value={row.created} max={max} kind="created" />
        <TrendBar value={row.resolved} max={max} kind="resolved" />
      </div>
    </div>
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
          <ReportSection title={`Trend (${days}d)`}>
            <div className="my-2 flex h-[90px] items-end gap-[2px]">
              {rows.map((row) => (
                <TrendDay key={row.date} row={row} max={max} />
              ))}
            </div>
            <div className="flex gap-2">
              <Badge intent="info" variant="outline" size="sm">created</Badge>
              <Badge intent="success" variant="outline" size="sm">resolved</Badge>
            </div>
          </ReportSection>
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
          <ReportSection title="Time to resolve">
            <Hint>No issues resolved in this window.</Hint>
          </ReportSection>
        ) : (
          <ReportSection title={`Time to resolve (${days}d)`}>
            {/* StatCards, like the summary above. These were the last three
                hand-rolled `.stat` divs in the app: same three numbers, same
                row, drawn by a second private copy of the treatment the
                summary had already replaced. The heading also names its
                window, because these three are the only numbers on the page
                that are NOT all-time and nothing said so. */}
            <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-3">
              <StatCard label="Resolved" value={data.resolvedCount} />
              <StatCard label="Average" value={humanMs(data.averageMs ?? 0)} />
              <StatCard label="Median" value={humanMs(data.medianMs ?? 0)} />
            </div>
          </ReportSection>
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
    <Page>
      <PageHeader>
        <PageHeader.Title>Reports</PageHeader.Title>
      </PageHeader>
      {/* Both filters read the same way. The product picker carried only an
          aria-label while "Window (days)" beside it showed a visible one, so
          the bar had one labelled control and one bare box whose meaning you
          inferred from its placeholder. */}
      <InlineForm>
        <Field className="flex-none">
          <Field.Label>Product</Field.Label>
          <FilterControl>
            <Select
              value={productId}
              onValueChange={(next) => setProductId(next ?? "")}
              placeholder="All products"
              aria-label="Product"
              renderValue={(id) => products.find((p) => p.id === id)?.name ?? id}
            >
              {products.map((p) => (
                <Select.Item key={p.id} value={p.id}>
                  {p.name}
                </Select.Item>
              ))}
            </Select>
          </FilterControl>
        </Field>
        <Field className="flex-none">
          <Field.Label>Window (days)</Field.Label>
          <NumberField
            min={1}
            max={365}
            value={days}
            onValueChange={(next) => setDays(Math.min(365, Math.max(1, next ?? 30)))}
          />
        </Field>
      </InlineForm>
      <SummarySection productId={productId} />
      {/* Paired across the page rather than stacked down it. Every one of these
          four is a narrow thing -- a two-column table, or three numbers -- and
          full-width bands left most of a 1440px page empty while pushing the
          later reports two screens down. The sidebar is gone; the page has the
          width to show them together, and comparing "which component" against
          "which person" side by side is the whole reason both exist. */}
      <ReportPair>
        <ByComponentSection productId={productId} productNames={productNames} />
        <ByAssigneeSection productId={productId} />
      </ReportPair>
      <ReportPair>
        <TrendSection productId={productId} days={days} />
        <TimeToResolveSection productId={productId} days={days} />
      </ReportPair>
    </Page>
  );
}
