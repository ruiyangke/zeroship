/*
 * StatsBand — a horizontal band of headline metrics.
 *
 * An optional header lead-in (eyebrow + title + description) above a
 * responsive grid of stat items. Each item is a large `value` (display type
 * scale) over a muted `label`, with an optional small description. A piece of
 * the `sections/` layer (after Hero, PricingTable, FeatureGrid): page-level
 * bands that compose the layout primitives + styled components.
 *
 * Like FeatureGrid, StatsBand exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case, the stats in one array:
 *        <StatsBand
 *          title="Trusted at scale"
 *          stats={[
 *            { id: "apps", value: "12k+", label: "Apps shipped" },
 *            { id: "uptime", value: "99.99%", label: "Uptime" },
 *            { id: "latency", value: "<50ms", label: "p99 latency" },
 *          ]}
 *        />
 *
 *   2. Compound parts — full control over a stat's composition / order:
 *        <StatsBand title="Trusted at scale">
 *          <StatsBand.Stat value="12k+" label="Apps shipped" />
 *          <StatsBand.Stat value="99.99%" label="Uptime">
 *            Rolling 90-day average.
 *          </StatsBand.Stat>
 *        </StatsBand>
 *
 * The two surfaces are ADDITIVE — there is NO suppression (the Hero rule).
 * The `stats` prop renders FIRST; any compound `<StatsBand.Stat>` children
 * then fall through AFTER, in document order. The root walks the children with
 * a recursive `flattenChildren` (Fragments descended) so a Fragment-wrapped
 * `<><StatsBand.Stat/></>` is never silently dropped. Mirrors the walk in
 * sections/FeatureGrid/FeatureGrid.tsx.
 *
 * Layout (dogfoods the layout primitives — never re-rolls grid/flex):
 *   - The root is a `<section data-slot="stats-band">` with generous vertical
 *     padding and NO heavy background (composable).
 *   - Inside sits a `Container` holding an optional header `Stack` above a
 *     `Grid` of stat items.
 *   - The Grid carries `columns` (2|3|4, default = stats count capped at 4) at
 *     wide widths; the `Grid` primitive ITSELF owns the collapse to a single
 *     stacked column below `--zs-bp-md`, so this band adds no local override.
 *
 * a11y:
 *   - The section header title is an `<h2>`; the `<section>` is
 *     `aria-labelledby` it ONLY when the title renders — the attr is gated on
 *     the heading actually rendering, so an absent, `false`, or empty (`""`)
 *     `title` never leaves an empty heading nor a dangling reference (the Hero
 *     R6 / FeatureGrid lesson). Used headerless the section carries NO
 *     aria-labelledby.
 *   - A stats row is NOT a heading outline: the stat `value` and `label` are
 *     plain TEXT, never headings — promoting "12k+" to an `<h3>` would pollute
 *     the document heading structure with non-heading content. Each stat is a
 *     `group` whose meaning lives in its visible label (color-not-alone is N/A;
 *     the label is always present text). forced-colors keeps text legible; the
 *     reduced-motion parity block is present.
 *
 * `data-slot` vocabulary (mirrors Hero / FeatureGrid):
 *   stats-band                  — the <section> root (overridable)
 *   stats-band-header           — the optional eyebrow + title + desc lead-in
 *   stats-band-title            — the optional section lead-in <h2>
 *   stats-band-items            — the responsive Grid of stats
 *   stats-band-stat             — each stat cell (group)
 *   stats-band-stat-value       — the large metric value (plain text)
 *   stats-band-stat-label       — the muted label (plain text)
 *   stats-band-stat-description — the optional small supporting line
 */
import {
  Children,
  Fragment,
  forwardRef,
  isValidElement,
  useId,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { classnames } from "../../components/_classnames";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Grid } from "../../layouts/Grid";
import { Stack } from "../../layouts/Stack";

/** Header + item text alignment for the stats band. */
export type StatsBandAlign = "center" | "start";

/** Column count at wide widths (collapses to 1 below `--zs-bp-md`). */
export type StatsBandColumns = 2 | 3 | 4;

/* ─── item ────────────────────────────────────────────────────────────── */

export interface StatItem {
  /** Stable id — the React key and item identity (else the index). */
  id?: string;
  /**
   * The headline metric — rendered large, in plain TEXT (not a heading). A
   * string like `"12k+"`, `"99.99%"`, or a node.
   */
  value: ReactNode;
  /** The metric's meaning — a muted label beneath the value, in plain text. */
  label: ReactNode;
  /** Optional small supporting line beneath the label (muted). */
  description?: ReactNode;
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface StatsBandProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /** Optional small label above the section title (eyebrow). */
  eyebrow?: ReactNode;

  /**
   * Optional section title lead-in. When a NON-EMPTY `title` is supplied it
   * renders as an `<h2>` AND becomes the section's `aria-labelledby` target —
   * the label attr is gated on the heading actually rendering, so an absent,
   * `false`, or empty (`""`) `title` never leaves an empty heading nor a
   * dangling reference (the Hero R6 / FeatureGrid lesson).
   */
  title?: ReactNode;

  /** Optional supporting paragraph under the title (muted). */
  description?: ReactNode;

  /**
   * Ergonomic mode: the stat items, in order. Rendered FIRST; any compound
   * `<StatsBand.Stat>` children fall through AFTER (the surfaces are additive
   * — no suppression).
   */
  stats?: StatItem[];

  /**
   * Column count at wide widths — `2`, `3`, or `4`. Default = the total stat
   * count, capped at `4`. The Grid promotes to this count at `--zs-bp-md` and
   * up; below it the Grid primitive itself collapses to a single stacked
   * column.
   */
  columns?: StatsBandColumns;

  /**
   * Header + item text alignment. `center` (default) centers the header and
   * each stat; `start` start-aligns them.
   */
  align?: StatsBandAlign;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `lg`.
   */
  size?: ContainerSize;

  /** Compound `<StatsBand.Stat>` parts (additive after `stats`). */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"stats-band"`. A composing section
   * can override it so consumers target the outer element via its own slot
   * vocabulary. Mirrors Card / Hero / FeatureGrid / Container.
   */
  "data-slot"?: string;
}

/* ─── compound part props ─────────────────────────────────────────────── */

export interface StatsBandStatProps {
  /** The headline metric — rendered large, in plain text (not a heading). */
  value: ReactNode;
  /** The metric's meaning — a muted label beneath the value. */
  label: ReactNode;
  /**
   * Optional small supporting line. Supplied via the `description` prop OR as
   * the part's `children` (the prop wins when both are present).
   */
  description?: ReactNode;
  /** Supporting line as children (alternative to `description`). */
  children?: ReactNode;
}

/* ─── child walk ──────────────────────────────────────────────────────── */

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><StatsBand.Stat/></>`) are visible to the normalizer.
 * Preserves document order and keys. Mirrors the walk in
 * sections/FeatureGrid/FeatureGrid.tsx. */
function flattenChildren(children: ReactNode): ReactNode[] {
  const out: ReactNode[] = [];
  Children.forEach(children, (child) => {
    if (isValidElement(child) && child.type === Fragment) {
      out.push(
        ...flattenChildren((child.props as { children?: ReactNode }).children),
      );
    } else {
      out.push(child);
    }
  });
  return out;
}

/* ─── shared item renderer ────────────────────────────────────────────── */

/* The single source of truth for a stat cell — both the `stats` prop path and
 * the compound `<StatsBand.Stat>` path funnel through here, so the two
 * surfaces render byte-identical cells. The value + label are plain TEXT (not
 * headings — a stats row is not part of the heading outline). The cell is a
 * `group` so AT can navigate stat-by-stat; the visible label carries the
 * meaning. */
interface ResolvedStat {
  key: string;
  value: ReactNode;
  label: ReactNode;
  description?: ReactNode;
}

function renderStat(stat: ResolvedStat) {
  const { value, label, description } = stat;
  return (
    <div
      key={stat.key}
      role="group"
      data-slot="stats-band-stat"
      className="zs-stats-band__stat"
    >
      <p
        data-slot="stats-band-stat-value"
        className="zs-stats-band__stat-value"
      >
        {value}
      </p>
      <p
        data-slot="stats-band-stat-label"
        className="zs-stats-band__stat-label"
      >
        {label}
      </p>
      {description != null ? (
        <p
          data-slot="stats-band-stat-description"
          className="zs-stats-band__stat-description"
        >
          {description}
        </p>
      ) : null}
    </div>
  );
}

/* ─── root ────────────────────────────────────────────────────────────── */

const StatsBandRoot = forwardRef<HTMLElement, StatsBandProps>(
  function StatsBandRoot(
    {
      eyebrow,
      title,
      description,
      stats,
      columns,
      align = "center",
      size = "lg",
      className,
      children,
      "data-slot": dataSlot = "stats-band",
      ...rest
    },
    ref,
  ) {
    const composedClassName = classnames("zs-stats-band", className);

    // Stable id the section uses for aria-labelledby when an optional `title`
    // lead-in renders. useId is SSR-safe + collision-free across multiple
    // StatsBands on a page.
    const titleId = useId();

    // ── Resolve the `stats` prop path ─────────────────────────────────────
    // Keys are NAMESPACED by source surface (`prop:` / `compound:`) so a prop
    // item id and a compound item key can never collide into a duplicate React
    // key in the merged list.
    const propStats: ResolvedStat[] = (stats ?? []).map((stat, index) => ({
      key: `prop:${stat.id ?? index}`,
      value: stat.value,
      label: stat.label,
      description: stat.description,
    }));

    // ── Resolve the compound `<StatsBand.Stat>` path (ADDITIVE) ───────────
    // One walk over the flattened children (Fragments descended) lifts every
    // `<StatsBand.Stat>` into a ResolvedStat. Compound stats fall through
    // AFTER the prop stats in document order — there is no suppression. The
    // supporting line comes from the `description` prop, else the part's
    // `children`.
    const compoundStats: ResolvedStat[] = [];
    flattenChildren(children).forEach((child, index) => {
      if (!isValidElement(child) || child.type !== StatsBandStat) return;
      const p = (child as ReactElement<StatsBandStatProps>).props;
      compoundStats.push({
        key: `compound:${(child as ReactElement).key ?? index}`,
        value: p.value,
        label: p.label,
        description: p.description ?? p.children,
      });
    });

    const allStats = [...propStats, ...compoundStats];

    // Default column count = the total stat count, capped at 4. An explicit
    // `columns` prop wins.
    const resolvedColumns: StatsBandColumns =
      columns ?? ((Math.min(allStats.length, 4) || 1) as StatsBandColumns);

    // The section is labelled ONLY when a real `title` heading renders — never
    // a dangling aria-labelledby (the Hero R6 / FeatureGrid lesson). A
    // RENDERABILITY guard, not a nullish check.
    const titleRenders = title != null && title !== false && title !== "";
    const hasHeader = titleRenders || eyebrow != null || description != null;

    return (
      <section
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot={dataSlot}
        data-align={align}
        aria-labelledby={titleRenders ? titleId : undefined}
        className={composedClassName}
      >
        <Container size={size} data-slot="stats-band-container">
          {hasHeader ? (
            <Stack
              gap={3}
              align={align === "center" ? "center" : "start"}
              data-slot="stats-band-header"
              className="zs-stats-band__header"
            >
              {eyebrow != null ? (
                <p className="zs-stats-band__eyebrow">{eyebrow}</p>
              ) : null}
              {titleRenders ? (
                <h2
                  id={titleId}
                  data-slot="stats-band-title"
                  className="zs-stats-band__title"
                >
                  {title}
                </h2>
              ) : null}
              {description != null ? (
                <p className="zs-stats-band__description">{description}</p>
              ) : null}
            </Stack>
          ) : null}

          <Grid
            columns={{ md: resolvedColumns }}
            gap={7}
            data-slot="stats-band-items"
            className="zs-stats-band__items"
          >
            {allStats.map((stat) => renderStat(stat))}
          </Grid>
        </Container>
      </section>
    );
  },
);
StatsBandRoot.displayName = "StatsBand";

/* ─── compound part ───────────────────────────────────────────────────── */

/* StatsBand.Stat is a MARKER: the root reads its props during the child walk
 * and renders the real cell itself (via `renderStat`) so the prop path and the
 * compound path produce identical markup. Rendering it standalone (outside a
 * `<StatsBand>`) is a misuse — it dev-warns and renders nothing rather than
 * emit orphan markup with no grid around it. */
function StatsBandStat(_props: StatsBandStatProps): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "StatsBand.Stat must be a direct (or Fragment-wrapped) child of " +
        "<StatsBand>; the root reads its props to render the stat cell. " +
        "Rendered standalone it produces nothing.",
    );
  }
  return null;
}
(StatsBandStat as { displayName?: string }).displayName = "StatsBand.Stat";

/* ─── public StatsBand namespace ──────────────────────────────────────── */

type StatsBandComponent = typeof StatsBandRoot & {
  Stat: typeof StatsBandStat;
};

export const StatsBand = StatsBandRoot as StatsBandComponent;
StatsBand.Stat = StatsBandStat;
