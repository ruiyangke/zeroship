/*
 * PricingTable — the monetization band.
 *
 * A responsive row of pricing TIERS. Each tier is a `Card` carrying a
 * name (heading), a price (amount + period), a one-line description, a
 * `Separator`, a feature list, and a CTA pinned to the card bottom so
 * the CTAs align across tiers of differing feature counts. One tier may
 * be FEATURED (a token accent ring + raised elevation + a "Most popular"
 * badge). The second piece of the `sections/` layer (after Hero).
 *
 * Like Hero, PricingTable exposes a DUAL surface:
 *
 *   1. Ergonomic props — the 80% case, the tiers in one array:
 *        <PricingTable
 *          tiers={[
 *            { id: "free", name: "Free", price: "$0", period: "/mo",
 *              description: "For trying things out.",
 *              features: [{ label: "1 project" },
 *                          { label: "Email support", included: false }],
 *              ctaLabel: "Get started", onCtaClick },
 *            { id: "pro", name: "Pro", price: "$29", period: "/mo",
 *              featured: true, badge: "Most popular", … },
 *          ]}
 *        />
 *
 *   2. Compound parts — full control over a tier's composition:
 *        <PricingTable>
 *          <PricingTable.Tier name="Pro" price="$29" period="/mo" featured
 *                             badge="Most popular" cta={<Button>Upgrade</Button>}>
 *            <PricingTable.Feature>Unlimited projects</PricingTable.Feature>
 *            <PricingTable.Feature included={false}>SSO</PricingTable.Feature>
 *          </PricingTable.Tier>
 *        </PricingTable>
 *
 * The two surfaces are ADDITIVE — there is NO suppression (the Hero
 * rule). The `tiers` prop renders first; any compound `<PricingTable.Tier>`
 * children then fall through AFTER, in document order. Supplying both
 * renders every tier from both surfaces. The root walks the children
 * with a recursive `flattenChildren` (Fragments descended) so a
 * Fragment-wrapped `<><PricingTable.Tier/></>` is never silently dropped.
 *
 * Layout (dogfoods the layout primitives — never re-rolls grid/flex):
 *   - The root is a `<section data-slot="pricing-table">` with generous
 *     vertical padding and NO heavy background (composable — the consumer
 *     paints the page backdrop).
 *   - Inside sits a `Container` (the single inline-width authority)
 *     holding a `Grid` of tier cards. The Grid carries N columns at wide
 *     widths (N = tier count, capped at 4 so 5+ tiers wrap rather than
 *     shrink to slivers); the `Grid` primitive itself owns the collapse to
 *     a SINGLE stacked column below `--zs-bp-md` (its responsive base count
 *     is 1; `columns={{ md }}` only promotes at ≥ the breakpoint), so this
 *     band adds no local collapse override.
 *   - Every tier `Card` is `align-items: stretch` in its track so the
 *     cards are EQUAL-HEIGHT; the in-card body is a column flexbox where
 *     the feature list grows (`flex: 1`) and the CTA sits in a trailing
 *     row — so CTAs align across tiers regardless of feature count.
 *
 * a11y:
 *   - Each tier name renders as a heading. The default level is `<h3>`
 *     (the band typically sits under a page `<h1>`/`<h2>`); `headingLevel`
 *     relevels every tier name in one place to match the document outline.
 *   - This section has NO single headline (it is a ROW of tiers, not one
 *     titled band), so the root `<section>` is NOT labelled by default —
 *     we never emit a dangling `aria-labelledby` (the Hero R6 lesson).
 *     A consumer wraps the band under their own page heading. IF an
 *     optional `title` lead-in is supplied, THEN the section is labelled
 *     by that real heading (the attr is gated on the heading rendering).
 *   - The feature list is a real `<ul>`/`<li>`. Inclusion is conveyed by
 *     an `Icon` (`Check` included / `Minus` excluded) PLUS a
 *     `.zs-visually-hidden` word ("Included" / "Not included") — NEVER by
 *     color or icon shape alone (color-blind + SR users). The icons are
 *     decorative (no `label`) because the hidden word carries the meaning.
 *   - CTAs are real `<Button>`s with discernible text. A featured tier
 *     ALWAYS carries a visible badge — a consumer `badge` if supplied, else
 *     a default "Most popular" `<Badge>`. "recommended" is real VISIBLE
 *     text, never conveyed by the accent ring / elevation alone.
 *
 * `data-slot` vocabulary (mirrors Card / Hero so consumers target regions
 * in CSS without leaning on the internal BEM class names):
 *   pricing-table        — the <section> root (overridable)
 *   pricing-table-title  — the optional section lead-in heading
 *   pricing-tier         — each tier <Card> (relabels Card's "card")
 *   pricing-tier-header  — the badge + name + price + description region
 *   pricing-price        — the price line (amount + muted period)
 *   pricing-features     — the feature <ul>
 *   pricing-feature      — each feature <li>
 *   pricing-cta          — the trailing CTA row (pinned to the card bottom)
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
import { Card } from "../../components/Card/Card";
import { Button } from "../../components/Button/Button";
import { Badge } from "../../components/Badge/Badge";
import { Icon } from "../../components/Icon/Icon";
import { Separator } from "../../components/Separator/Separator";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Grid } from "../../layouts/Grid";
import { Stack } from "../../layouts/Stack";
import { Check, Minus } from "lucide-react";

/**
 * Heading level for the tier names. The band typically sits under a page
 * `<h1>`/`<h2>`, so tier names default to `<h3>`; relevel here in one
 * place to match the surrounding document outline.
 */
export type PricingHeadingLevel = "h2" | "h3" | "h4";

/* The cap on the responsive column count. 5+ tiers wrap to a second row
 * rather than shrinking every card to an unreadable sliver. */
const MAX_COLUMNS = 4;

/* ─── feature ─────────────────────────────────────────────────────────── */

export interface PricingFeature {
  /** The feature text. */
  label: ReactNode;
  /**
   * Is this feature INCLUDED in the tier? Default `true` (a `Check` icon +
   * the visually-hidden word "Included"). `false` renders a `Minus` icon +
   * "Not included" and reads muted. Inclusion is ALWAYS conveyed by the
   * icon + the hidden word, never by color alone.
   */
  included?: boolean;
  /** Optional trailing note beside the label (e.g. "up to 10k rows"). */
  note?: ReactNode;
}

/* ─── tier ────────────────────────────────────────────────────────────── */

export interface PricingTier {
  /** Stable id — the React key and the tier identity. */
  id: string;
  /** Tier name — renders as a heading (`headingLevel`, default `<h3>`). */
  name: ReactNode;
  /** Price amount, already formatted (e.g. "$29", "Free", "Custom"). */
  price: ReactNode;
  /** Period suffix beside the price (e.g. "/mo"). Optional, rendered muted. */
  period?: ReactNode;
  /** One-line positioning copy under the price. */
  description?: ReactNode;
  /** The feature list. Each entry is a {@link PricingFeature}. */
  features?: PricingFeature[];
  /**
   * The CTA — a consumer-supplied `<Button>` (or any node). When omitted,
   * `ctaLabel` renders a default `<Button>` instead. The CTA is pinned to
   * the card bottom so it aligns across tiers of differing feature counts.
   */
  cta?: ReactNode;
  /** CTA label used when `cta` is not supplied → renders a default `<Button>`. */
  ctaLabel?: ReactNode;
  /** Click handler for the default (`ctaLabel`) `<Button>`. */
  onCtaClick?: () => void;
  /**
   * Highlight this tier — a raised elevation + a token accent ring. The
   * featured tier also carries the `badge`. Token-pure; no raw color.
   */
  featured?: boolean;
  /**
   * Small badge on a featured tier (e.g. "Most popular"). Pass a string
   * (rendered as a `<Badge>`) or a `<Badge>`/node for full control. The
   * badge text is real VISIBLE text — it is the accessible signal for
   * "recommended", never the ring/elevation alone. When a tier is
   * `featured` and `badge` is omitted, a visible "Most popular" `<Badge>`
   * is supplied by default so a featured tier never signals "recommended"
   * by the accent ring / elevation alone.
   */
  badge?: ReactNode;
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface PricingTableProps
  extends Omit<ComponentPropsWithoutRef<"section">, "title"> {
  /**
   * Ergonomic mode: the tiers, left→right. Rendered FIRST; any compound
   * `<PricingTable.Tier>` children fall through AFTER (the surfaces are
   * additive — no suppression).
   */
  tiers?: PricingTier[];

  /**
   * Optional section lead-in heading. The band has no single headline by
   * default (it is a row of tiers), so the `<section>` is unlabelled. When
   * a NON-EMPTY `title` is supplied it renders as an `<h2>` AND becomes the
   * section's `aria-labelledby` target — the label attr is gated on the
   * heading actually rendering, so an absent, `false`, or empty (`""`)
   * `title` (e.g. the `showTitle && "…"` idiom) never leaves an empty
   * heading nor a dangling reference (the Hero R6 lesson).
   */
  title?: ReactNode;

  /** Optional muted lead-in paragraph beneath the `title`. */
  description?: ReactNode;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `lg`.
   */
  size?: ContainerSize;

  /**
   * Heading level for EVERY tier name. Default `h3` (the band sits under a
   * page `<h1>`/`<h2>`). Relevel here to match the document outline.
   */
  headingLevel?: PricingHeadingLevel;

  /** Compound `<PricingTable.Tier>` parts (additive after `tiers`). */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"pricing-table"`. A composing
   * section can override it so consumers target the outer element via its
   * own slot vocabulary. Mirrors Card / Hero / Container.
   */
  "data-slot"?: string;
}

/* ─── compound part props ─────────────────────────────────────────────── */

export interface PricingTableTierProps {
  /** Tier name — renders as a heading (level inherited from the root). */
  name: ReactNode;
  /** Price amount, already formatted. */
  price: ReactNode;
  /** Period suffix beside the price (rendered muted). */
  period?: ReactNode;
  /** One-line positioning copy under the price. */
  description?: ReactNode;
  /** CTA node (a `<Button>`); falls back to `ctaLabel`. */
  cta?: ReactNode;
  /** CTA label when `cta` is not supplied → default `<Button>`. */
  ctaLabel?: ReactNode;
  /** Click handler for the default (`ctaLabel`) `<Button>`. */
  onCtaClick?: () => void;
  /** Highlight this tier (accent ring + elevation). */
  featured?: boolean;
  /**
   * Badge on a featured tier (string → `<Badge>`, or a node). When omitted
   * on a `featured` tier, defaults to a visible "Most popular" `<Badge>` so
   * "recommended" is never the ring/elevation alone.
   */
  badge?: ReactNode;
  /** Feature rows — `<PricingTable.Feature>` parts (and/or array `features`). */
  features?: PricingFeature[];
  /** Compound `<PricingTable.Feature>` children (additive after `features`). */
  children?: ReactNode;
}

export interface PricingTableFeatureProps {
  /** Is this feature included? Default `true`. `false` → muted + "Not included". */
  included?: boolean;
  /** Optional trailing note beside the label. */
  note?: ReactNode;
  /** The feature text. */
  children?: ReactNode;
}

/* ─── child walk ──────────────────────────────────────────────────────── */

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><PricingTable.Tier/></>`) are visible to the
 * normalizer. Preserves document order and keys. Mirrors the walk in
 * sections/Hero/Hero.tsx. */
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

/* ─── feature row ─────────────────────────────────────────────────────── */

/* One `<li>` in a tier's feature list: an inclusion icon + a
 * visually-hidden word + the label + an optional muted note. The icon is
 * decorative (no `label`) — the hidden word is the AT-facing signal, so
 * inclusion is never conveyed by color/icon shape alone. */
function FeatureRow({
  label,
  included = true,
  note,
}: PricingFeature) {
  return (
    <li
      data-slot="pricing-feature"
      data-included={included ? "" : undefined}
      className="zs-pricing__feature"
    >
      <Icon
        as={included ? Check : Minus}
        size="sm"
        className="zs-pricing__feature-icon"
      />
      <span className="zs-visually-hidden">
        {included ? "Included" : "Not included"}
      </span>
      <span className="zs-pricing__feature-label">{label}</span>
      {note != null ? (
        <span className="zs-pricing__feature-note">{note}</span>
      ) : null}
    </li>
  );
}

/* ─── shared tier renderer ────────────────────────────────────────────── */

/* The single source of truth for a tier Card — both the `tiers` prop path
 * and the compound `<PricingTable.Tier>` path funnel through here, so the
 * two surfaces render byte-identical cards. `Heading` is the resolved tag
 * from the root's `headingLevel`. */
interface ResolvedTier {
  key: string;
  name: ReactNode;
  price: ReactNode;
  period?: ReactNode;
  description?: ReactNode;
  features: PricingFeature[];
  cta?: ReactNode;
  ctaLabel?: ReactNode;
  onCtaClick?: () => void;
  featured?: boolean;
  badge?: ReactNode;
}

function renderTier(tier: ResolvedTier, Heading: PricingHeadingLevel) {
  const {
    name,
    price,
    period,
    description,
    features,
    cta,
    ctaLabel,
    onCtaClick,
    featured,
    badge,
  } = tier;

  // The CTA: a consumer node wins; otherwise a default filled Button from
  // ctaLabel. A featured tier's default CTA reads `filled` (the loud
  // primary); the rest read `tinted` so the featured CTA stands out.
  const ctaNode =
    cta != null ? (
      cta
    ) : ctaLabel != null ? (
      <Button
        variant={featured ? "filled" : "tinted"}
        onClick={onCtaClick}
        className="zs-pricing__cta-button"
      >
        {ctaLabel}
      </Button>
    ) : null;

  // The badge: a string is wrapped in an accent Badge; a node passes
  // through (e.g. a consumer <Badge> with custom intent). A FEATURED tier
  // with NO badge defaults to a visible "Most popular" Badge — "recommended"
  // is then ALWAYS carried by real visible/AT-readable text, never by the
  // accent ring / elevation alone (the house "never color alone" rule:
  // color-blind + SR users get the signal). A consumer-supplied `badge`
  // (string or node) overrides this default.
  const resolvedBadge = badge ?? (featured ? "Most popular" : null);
  const badgeNode =
    typeof resolvedBadge === "string" ? (
      <Badge intent="info" variant="solid" size="sm">
        {resolvedBadge}
      </Badge>
    ) : (
      resolvedBadge ?? null
    );

  return (
    <Card
      key={tier.key}
      variant={featured ? "elevated" : "outline"}
      data-slot="pricing-tier"
      data-featured={featured ? "" : undefined}
      className="zs-pricing__tier"
    >
      <div data-slot="pricing-tier-header" className="zs-pricing__header">
        {badgeNode != null ? (
          <div className="zs-pricing__badge">{badgeNode}</div>
        ) : null}
        <Heading className="zs-pricing__name">{name}</Heading>
        <p data-slot="pricing-price" className="zs-pricing__price">
          <span className="zs-pricing__amount">{price}</span>
          {period != null ? (
            <span className="zs-pricing__period">{period}</span>
          ) : null}
        </p>
        {description != null ? (
          <p className="zs-pricing__description">{description}</p>
        ) : null}
      </div>

      {/* The Separator only earns its hairline when there is a feature list
          beneath it — no dangling rule above an empty grow region. */}
      {features.length > 0 ? (
        <>
          <Separator className="zs-pricing__divider" />
          <ul data-slot="pricing-features" className="zs-pricing__features">
            {features.map((feature, index) => (
              <FeatureRow key={index} {...feature} />
            ))}
          </ul>
        </>
      ) : (
        // No features: keep the grow region present (so the CTA still pins to
        // the card bottom for equal-height alignment) and tag it with the
        // same `pricing-features` slot for consistent targeting — but emit no
        // Separator (no hairline above an empty region).
        <div
          data-slot="pricing-features"
          className="zs-pricing__features"
          aria-hidden="true"
        />
      )}

      {ctaNode != null ? (
        <div data-slot="pricing-cta" className="zs-pricing__cta">
          {ctaNode}
        </div>
      ) : null}
    </Card>
  );
}

/* ─── root ────────────────────────────────────────────────────────────── */

const PricingTableRoot = forwardRef<HTMLElement, PricingTableProps>(
  function PricingTableRoot(
    {
      tiers,
      title,
      description,
      size = "lg",
      headingLevel = "h3",
      className,
      children,
      "data-slot": dataSlot = "pricing-table",
      ...rest
    },
    ref,
  ) {
    const composedClassName = classnames("zs-pricing", className);

    // Stable id the section uses for aria-labelledby when an optional `title`
    // lead-in renders. useId is SSR-safe + collision-free across multiple
    // PricingTables on a page.
    const titleId = useId();

    // ── Resolve the `tiers` prop path ────────────────────────────────────
    // Keys are NAMESPACED by source surface (`prop:` here, `compound:`
    // below) so a prop tier id and a compound tier key can never collide
    // into a duplicate React key in the merged `allTiers` map.
    const propTiers: ResolvedTier[] = (tiers ?? []).map((tier) => ({
      key: `prop:${tier.id}`,
      name: tier.name,
      price: tier.price,
      period: tier.period,
      description: tier.description,
      features: tier.features ?? [],
      cta: tier.cta,
      ctaLabel: tier.ctaLabel,
      onCtaClick: tier.onCtaClick,
      featured: tier.featured,
      badge: tier.badge,
    }));

    // ── Resolve the compound `<PricingTable.Tier>` path (ADDITIVE) ────────
    // One walk over the flattened children (Fragments descended) lifts every
    // `<PricingTable.Tier>` into a ResolvedTier, collecting its array
    // `features` plus any `<PricingTable.Feature>` children (also additive,
    // Fragments descended). Compound tiers fall through AFTER the prop tiers
    // in document order — there is no suppression.
    const compoundTiers: ResolvedTier[] = [];
    flattenChildren(children).forEach((child, tierIndex) => {
      if (!isValidElement(child) || child.type !== PricingTableTier) return;
      const t = (child as ReactElement<PricingTableTierProps>).props;

      const featureChildren: PricingFeature[] = [];
      flattenChildren(t.children).forEach((fchild) => {
        if (!isValidElement(fchild) || fchild.type !== PricingTableFeature) {
          return;
        }
        const f = (fchild as ReactElement<PricingTableFeatureProps>).props;
        featureChildren.push({
          label: f.children,
          included: f.included,
          note: f.note,
        });
      });

      compoundTiers.push({
        // Namespaced by source surface (see propTiers above) so a compound
        // tier key can never collide with a prop tier id.
        key: `compound:${(child as ReactElement).key ?? tierIndex}`,
        name: t.name,
        price: t.price,
        period: t.period,
        description: t.description,
        // Array `features` first, then `<PricingTable.Feature>` children.
        features: [...(t.features ?? []), ...featureChildren],
        cta: t.cta,
        ctaLabel: t.ctaLabel,
        onCtaClick: t.onCtaClick,
        featured: t.featured,
        badge: t.badge,
      });
    });

    const allTiers = [...propTiers, ...compoundTiers];

    // Responsive column count: one column per tier, capped at MAX_COLUMNS so
    // 5+ tiers wrap rather than shrink to slivers. The Grid promotes to this
    // count at the --zs-bp-md breakpoint and up; below it the Grid primitive
    // itself collapses to a single stacked column (its responsive base count
    // is 1), so no local override is needed.
    const columnCount = Math.min(Math.max(allTiers.length, 1), MAX_COLUMNS);

    // The section is labelled ONLY when a real `title` heading renders — we
    // never emit a dangling aria-labelledby (the Hero R6 lesson). The band
    // has no single headline by default (it is a row of tiers), so an absent
    // `title` leaves the section unlabelled for a consumer's page heading.
    //
    // A RENDERABILITY guard, not a nullish check: the common conditional
    // idiom `title={showTitle && "Pricing"}` yields `title={false}` when the
    // flag is off, and `title=""` is likewise empty. `false`, `null`,
    // `undefined`, and `""` all mean "no title" — gate BOTH the <h2> render
    // and the aria-labelledby on it so a falsy/empty title never leaves an
    // empty heading nor a dangling label reference.
    const titleRenders = title != null && title !== false && title !== "";

    return (
      <section
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot={dataSlot}
        aria-labelledby={titleRenders ? titleId : undefined}
        className={composedClassName}
      >
        <Container size={size} data-slot="pricing-table-container">
          {titleRenders || description != null ? (
            <Stack
              gap={3}
              align="center"
              data-slot="pricing-table-lead"
              className="zs-pricing__lead"
            >
              {titleRenders ? (
                <h2
                  id={titleId}
                  data-slot="pricing-table-title"
                  className="zs-pricing__title"
                >
                  {title}
                </h2>
              ) : null}
              {description != null ? (
                <p className="zs-pricing__lead-description">{description}</p>
              ) : null}
            </Stack>
          ) : null}

          <Grid
            columns={{ md: columnCount }}
            gap={5}
            align="stretch"
            data-slot="pricing-table-grid"
            className="zs-pricing__grid"
          >
            {allTiers.map((tier) => renderTier(tier, headingLevel))}
          </Grid>
        </Container>
      </section>
    );
  },
);
PricingTableRoot.displayName = "PricingTable";

/* ─── compound parts ──────────────────────────────────────────────────── */

/* These two parts are MARKERS: the root reads their props during the child
 * walk and renders the real tier/feature itself (via `renderTier` /
 * `FeatureRow`) so the prop path and the compound path produce identical
 * markup. Rendering them standalone (outside a `<PricingTable>`) is a
 * misuse — they dev-warn and render nothing rather than emit orphan markup
 * with no tier card around it. */

function PricingTableTier(_props: PricingTableTierProps): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "PricingTable.Tier must be a direct (or Fragment-wrapped) child of " +
        "<PricingTable>; the root reads its props to render the tier card. " +
        "Rendered standalone it produces nothing.",
    );
  }
  return null;
}
PricingTableTier.displayName = "PricingTable.Tier";

function PricingTableFeature(
  _props: PricingTableFeatureProps,
): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "PricingTable.Feature must be a child of <PricingTable.Tier>; the root " +
        "reads its props to render the feature row. Rendered standalone it " +
        "produces nothing.",
    );
  }
  return null;
}
PricingTableFeature.displayName = "PricingTable.Feature";

/* ─── public PricingTable namespace ───────────────────────────────────── */

type PricingTableComponent = typeof PricingTableRoot & {
  Tier: typeof PricingTableTier;
  Feature: typeof PricingTableFeature;
};

export const PricingTable = PricingTableRoot as PricingTableComponent;
PricingTable.Tier = PricingTableTier;
PricingTable.Feature = PricingTableFeature;
