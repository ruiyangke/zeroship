/*
 * FeatureGrid — the marketing "features" band.
 *
 * An optional header lead-in (eyebrow + title + description) above a
 * responsive grid of feature items. Each item is a decorative Lucide
 * `Icon` in a tinted badge + a title (heading) + a short description.
 * The third piece of the `sections/` layer (after Hero, PricingTable):
 * page-level bands that compose the layout primitives + styled components
 * into a marketing/content surface.
 *
 * Like Hero / PricingTable, FeatureGrid exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case, the items in one array:
 *        <FeatureGrid
 *          eyebrow="Why us"
 *          title="Everything you need to ship"
 *          description="A platform that handles the boring parts."
 *          features={[
 *            { id: "fast", icon: <Icon as={Zap} size="lg" />,
 *              title: "Fast", description: "Sub-second cold starts." },
 *            { id: "safe", icon: <Icon as={Shield} size="lg" />,
 *              title: "Secure", description: "Isolated per tenant." },
 *          ]}
 *        />
 *
 *   2. Compound parts — full control over an item's composition / order:
 *        <FeatureGrid title="Features">
 *          <FeatureGrid.Item icon={<Icon as={Zap} size="lg" />} title="Fast">
 *            Sub-second cold starts.
 *          </FeatureGrid.Item>
 *        </FeatureGrid>
 *
 * The two surfaces are ADDITIVE — there is NO suppression (the Hero rule).
 * The `features` prop renders FIRST; any compound `<FeatureGrid.Item>`
 * children then fall through AFTER, in document order. Supplying both
 * renders every item from both surfaces. The root walks the children with
 * a recursive `flattenChildren` (Fragments descended) so a Fragment-wrapped
 * `<><FeatureGrid.Item/></>` is never silently dropped. Mirrors the walk in
 * sections/Hero/Hero.tsx + sections/PricingTable/PricingTable.tsx.
 *
 * Layout (dogfoods the layout primitives — never re-rolls grid/flex):
 *   - The root is a `<section data-slot="feature-grid">` with generous
 *     vertical padding and NO heavy background (composable — the consumer
 *     paints the page backdrop).
 *   - Inside sits a `Container` (the single inline-width authority) holding
 *     an optional header `Stack` (eyebrow → `<h2>` → description, aligned
 *     per `align`, capped to a readable measure) above a `Grid` of items.
 *   - The Grid carries `columns` (2|3|4, default 3) at wide widths; the
 *     `Grid` primitive ITSELF owns the collapse to a SINGLE stacked column
 *     below `--zs-bp-md` (its responsive base count is 1; `columns={{ md }}`
 *     only promotes at ≥ the breakpoint), so this band adds no local
 *     collapse override.
 *
 * a11y:
 *   - The section header title is an `<h2>` (the band sits under a page
 *     `<h1>`); the `<section>` is `aria-labelledby` it ONLY when the title
 *     renders — the attr is gated on the heading actually rendering, so an
 *     absent, `false`, or empty (`""`) `title` (e.g. the `showTitle && "…"`
 *     idiom) never leaves an empty heading nor a dangling reference (the
 *     Hero R6 / PricingTable lesson). Used headerless the section carries
 *     NO aria-labelledby — a consumer wraps it under their own page heading.
 *   - Each feature title is a real `<h3>`. The icon is decorative
 *     (`aria-hidden` via the Icon no-label path) — the title carries the
 *     meaning. forced-colors: text + icon badges stay legible; the
 *     reduced-motion parity block is present.
 *
 * `data-slot` vocabulary (mirrors Card / Hero / PricingTable so consumers
 * target regions in CSS without leaning on the internal BEM class names):
 *   feature-grid                  — the <section> root (overridable)
 *   feature-grid-header           — the optional eyebrow + title + desc lead-in
 *   feature-grid-title            — the optional section lead-in <h2>
 *   feature-grid-items            — the responsive Grid of items
 *   feature-grid-item             — each feature cell
 *   feature-grid-item-icon        — the tinted icon badge
 *   feature-grid-item-title       — the feature title <h3>
 *   feature-grid-item-description — the muted supporting line
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

/** Header + item text alignment for the feature band. */
export type FeatureGridAlign = "center" | "start";

/** Column count at wide widths (collapses to 1 below `--zs-bp-md`). */
export type FeatureGridColumns = 2 | 3 | 4;

/* ─── item ────────────────────────────────────────────────────────────── */

export interface FeatureItem {
  /** Stable id — the React key and item identity (else the index). */
  id?: string;
  /**
   * The feature icon — typically an `<Icon as={LucideGlyph} size="lg">`
   * (the consumer passes the glyph). Rendered inside a tinted badge and
   * decorative (the Icon no-label path makes it `aria-hidden`) — the title
   * carries the meaning.
   */
  icon?: ReactNode;
  /** Feature name — renders as an `<h3>` heading. */
  title: ReactNode;
  /** Short supporting line beneath the title (muted). */
  description?: ReactNode;
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface FeatureGridProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /**
   * Optional small label above the section title (eyebrow). Plain text or
   * a `Badge`/node. Part of the optional header lead-in.
   */
  eyebrow?: ReactNode;

  /**
   * Optional section title lead-in. The band has no single headline by
   * default, so the `<section>` is unlabelled. When a NON-EMPTY `title` is
   * supplied it renders as an `<h2>` AND becomes the section's
   * `aria-labelledby` target — the label attr is gated on the heading
   * actually rendering, so an absent, `false`, or empty (`""`) `title`
   * (e.g. the `showTitle && "…"` idiom) never leaves an empty heading nor a
   * dangling reference (the Hero R6 / PricingTable lesson).
   */
  title?: ReactNode;

  /** Optional supporting paragraph under the title (muted). */
  description?: ReactNode;

  /**
   * Ergonomic mode: the feature items, in order. Rendered FIRST; any
   * compound `<FeatureGrid.Item>` children fall through AFTER (the surfaces
   * are additive — no suppression).
   */
  features?: FeatureItem[];

  /**
   * Column count at wide widths — `2`, `3` (default), or `4`. The Grid
   * promotes to this count at `--zs-bp-md` and up; below it the Grid
   * primitive itself collapses to a single stacked column.
   */
  columns?: FeatureGridColumns;

  /**
   * Header + item text alignment. `center` (default) centers the header and
   * each item's icon + text; `start` start-aligns them.
   */
  align?: FeatureGridAlign;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `lg`.
   */
  size?: ContainerSize;

  /** Compound `<FeatureGrid.Item>` parts (additive after `features`). */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"feature-grid"`. A composing
   * section can override it so consumers target the outer element via its
   * own slot vocabulary. Mirrors Card / Hero / PricingTable / Container.
   */
  "data-slot"?: string;
}

/* ─── compound part props ─────────────────────────────────────────────── */

export interface FeatureGridItemProps {
  /**
   * The feature icon — typically an `<Icon as={LucideGlyph} size="lg">`.
   * Rendered inside a tinted badge and decorative (the Icon no-label path).
   */
  icon?: ReactNode;
  /** Feature name — renders as an `<h3>` heading. */
  title: ReactNode;
  /**
   * Short supporting line. Supplied via the `description` prop OR as the
   * part's `children` (the prop wins when both are present).
   */
  description?: ReactNode;
  /** Supporting line as children (alternative to `description`). */
  children?: ReactNode;
}

/* ─── child walk ──────────────────────────────────────────────────────── */

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><FeatureGrid.Item/></>`) are visible to the
 * normalizer. Preserves document order and keys. Mirrors the walk in
 * sections/Hero/Hero.tsx + sections/PricingTable/PricingTable.tsx. */
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

/* The single source of truth for a feature cell — both the `features` prop
 * path and the compound `<FeatureGrid.Item>` path funnel through here, so
 * the two surfaces render byte-identical cells. The icon badge is omitted
 * entirely when no icon is supplied (no empty badge box). The title is an
 * `<h3>`; the icon is decorative (the Icon no-label path makes it
 * aria-hidden), so the heading is the AT-facing signal. */
interface ResolvedItem {
  key: string;
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
}

function renderItem(item: ResolvedItem) {
  const { icon, title, description } = item;
  return (
    <div
      key={item.key}
      data-slot="feature-grid-item"
      className="zs-feature-grid__item"
    >
      {icon != null ? (
        <div
          data-slot="feature-grid-item-icon"
          className="zs-feature-grid__item-icon"
        >
          {icon}
        </div>
      ) : null}
      <h3
        data-slot="feature-grid-item-title"
        className="zs-feature-grid__item-title"
      >
        {title}
      </h3>
      {description != null ? (
        <p
          data-slot="feature-grid-item-description"
          className="zs-feature-grid__item-description"
        >
          {description}
        </p>
      ) : null}
    </div>
  );
}

/* ─── root ────────────────────────────────────────────────────────────── */

const FeatureGridRoot = forwardRef<HTMLElement, FeatureGridProps>(
  function FeatureGridRoot(
    {
      eyebrow,
      title,
      description,
      features,
      columns = 3,
      align = "center",
      size = "lg",
      className,
      children,
      "data-slot": dataSlot = "feature-grid",
      ...rest
    },
    ref,
  ) {
    const composedClassName = classnames("zs-feature-grid", className);

    // Stable id the section uses for aria-labelledby when an optional `title`
    // lead-in renders. useId is SSR-safe + collision-free across multiple
    // FeatureGrids on a page.
    const titleId = useId();

    // ── Resolve the `features` prop path ──────────────────────────────────
    // Keys are NAMESPACED by source surface (`prop:` here, `compound:`
    // below) so a prop item id and a compound item key can never collide
    // into a duplicate React key in the merged `allItems` list.
    const propItems: ResolvedItem[] = (features ?? []).map((item, index) => ({
      key: `prop:${item.id ?? index}`,
      icon: item.icon,
      title: item.title,
      description: item.description,
    }));

    // ── Resolve the compound `<FeatureGrid.Item>` path (ADDITIVE) ─────────
    // One walk over the flattened children (Fragments descended) lifts every
    // `<FeatureGrid.Item>` into a ResolvedItem. Compound items fall through
    // AFTER the prop items in document order — there is no suppression. The
    // supporting line comes from the `description` prop, else the part's
    // `children`.
    const compoundItems: ResolvedItem[] = [];
    flattenChildren(children).forEach((child, index) => {
      if (!isValidElement(child) || child.type !== FeatureGridItem) return;
      const p = (child as ReactElement<FeatureGridItemProps>).props;
      compoundItems.push({
        // Namespaced by source surface (see propItems above) so a compound
        // item key can never collide with a prop item id.
        key: `compound:${(child as ReactElement).key ?? index}`,
        icon: p.icon,
        title: p.title,
        description: p.description ?? p.children,
      });
    });

    const allItems = [...propItems, ...compoundItems];

    // The section is labelled ONLY when a real `title` heading renders — we
    // never emit a dangling aria-labelledby (the Hero R6 / PricingTable
    // lesson). A RENDERABILITY guard, not a nullish check: the conditional
    // idiom `title={showTitle && "Features"}` yields `title={false}` when the
    // flag is off, and `title=""` is likewise empty. `false`, `null`,
    // `undefined`, and `""` all mean "no title" — gate BOTH the <h2> render
    // and the aria-labelledby on it.
    const titleRenders = title != null && title !== false && title !== "";
    const hasHeader =
      titleRenders || eyebrow != null || description != null;

    return (
      <section
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot={dataSlot}
        data-align={align}
        aria-labelledby={titleRenders ? titleId : undefined}
        className={composedClassName}
      >
        <Container size={size} data-slot="feature-grid-container">
          {hasHeader ? (
            <Stack
              gap={3}
              align={align === "center" ? "center" : "start"}
              data-slot="feature-grid-header"
              className="zs-feature-grid__header"
            >
              {eyebrow != null ? (
                <p className="zs-feature-grid__eyebrow">{eyebrow}</p>
              ) : null}
              {titleRenders ? (
                <h2
                  id={titleId}
                  data-slot="feature-grid-title"
                  className="zs-feature-grid__title"
                >
                  {title}
                </h2>
              ) : null}
              {description != null ? (
                <p className="zs-feature-grid__description">{description}</p>
              ) : null}
            </Stack>
          ) : null}

          <Grid
            columns={{ md: columns }}
            gap={7}
            data-slot="feature-grid-items"
            className="zs-feature-grid__items"
          >
            {allItems.map((item) => renderItem(item))}
          </Grid>
        </Container>
      </section>
    );
  },
);
FeatureGridRoot.displayName = "FeatureGrid";

/* ─── compound part ───────────────────────────────────────────────────── */

/* FeatureGrid.Item is a MARKER: the root reads its props during the child
 * walk and renders the real cell itself (via `renderItem`) so the prop path
 * and the compound path produce identical markup. Rendering it standalone
 * (outside a `<FeatureGrid>`) is a misuse — it dev-warns and renders nothing
 * rather than emit orphan markup with no grid around it. */
function FeatureGridItem(_props: FeatureGridItemProps): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "FeatureGrid.Item must be a direct (or Fragment-wrapped) child of " +
        "<FeatureGrid>; the root reads its props to render the feature cell. " +
        "Rendered standalone it produces nothing.",
    );
  }
  return null;
}
FeatureGridItem.displayName = "FeatureGrid.Item";

/* ─── public FeatureGrid namespace ────────────────────────────────────── */

type FeatureGridComponent = typeof FeatureGridRoot & {
  Item: typeof FeatureGridItem;
};

export const FeatureGrid = FeatureGridRoot as FeatureGridComponent;
FeatureGrid.Item = FeatureGridItem;
