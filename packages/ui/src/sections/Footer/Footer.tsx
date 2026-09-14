/*
 * Footer — the site footer band.
 *
 * The bottom-of-page landmark: a brand/blurb block beside columns of link
 * groups, a `Separator`, then a bottom bar (copyright + an optional actions
 * slot for social / secondary links). The final piece of the `sections/`
 * layer: page-level bands that compose the layout primitives + styled
 * components.
 *
 * The root renders a REAL `<footer>` element — the `contentinfo` landmark. A
 * page has exactly one such footer, and this band IS it, so the landmark is
 * correct (no extra wrapper, no `role` override needed; `<footer>` at the
 * document level is `contentinfo` natively).
 *
 * Like FeatureGrid / StatsBand / Faq, Footer exposes a DUAL surface for its
 * repeating list (the link columns):
 *
 *   1. Ergonomic props — the 80% case, the columns in one array:
 *        <Footer
 *          brand={<Logo />}
 *          description="Ship software without writing code."
 *          columns={[
 *            { id: "product", title: "Product",
 *              links: [{ label: "Pricing", href: "/pricing" }] },
 *          ]}
 *          copyright="© 2026 zeroship"
 *        />
 *
 *   2. Compound parts — full control over a column's composition / order:
 *        <Footer brand={<Logo />} copyright="© 2026 zeroship">
 *          <Footer.Column title="Product">
 *            <a href="/pricing">Pricing</a>
 *          </Footer.Column>
 *        </Footer>
 *
 * The two surfaces are ADDITIVE — there is NO suppression (the Hero rule). The
 * `columns` prop renders FIRST; any compound `<Footer.Column>` children fall
 * through AFTER, in document order. The root walks the children with a
 * recursive `flattenChildren` (Fragments descended). Mirrors
 * sections/FeatureGrid + sections/StatsBand + sections/Faq.
 *
 * Layout:
 *   - The root is a `<footer data-slot="footer">` with generous vertical
 *     padding and NO heavy background (composable).
 *   - Inside sits a `Container` holding: a top row = the brand+description
 *     block beside the link-group columns (a Grid that collapses to stacked
 *     below `--zs-bp-md` — the Grid owns the collapse); a `Separator`; then a
 *     bottom bar (copyright at the inline-start, actions at the inline-end,
 *     wraps on narrow widths).
 *   - Each column: a `<h3>` title + a `<ul>` of real `<a href>` links.
 *
 * a11y:
 *   - The root `<footer>` is the page `contentinfo` landmark (exactly one per
 *     page; the band IS the footer, so that's correct). There is NO
 *     aria-labelledby gating here — `<footer>` is a landmark by role, not by a
 *     heading reference; the column `<h3>`s structure the link groups.
 *   - Column titles are real `<h3>` headings; link lists are real `<ul>`/`<li>`
 *     of `<a href>` anchors (the house link treatment). forced-colors keeps
 *     text/links/separator legible; the reduced-motion parity block is present.
 *
 * `data-slot` vocabulary:
 *   footer               — the <footer> root (overridable)
 *   footer-brand         — the brand/wordmark block
 *   footer-description   — the short blurb under the brand
 *   footer-columns       — the link-group Grid
 *   footer-column        — each link group
 *   footer-column-title  — the column <h3>
 *   footer-links         — the column's <ul> of links
 *   footer-bottom        — the bottom bar (copyright + actions)
 *   footer-copyright     — the bottom-bar start (copyright)
 *   footer-actions       — the bottom-bar end (social / secondary links slot)
 */
import {
  Children,
  Fragment,
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { classnames } from "../../components/_classnames";
import { Separator } from "../../components/Separator";
import { Container, type ContainerSize } from "../../layouts/Container";
import { Grid } from "../../layouts/Grid";
import type { SectionTone } from "../_tone";

/* ─── link + column shapes ────────────────────────────────────────────── */

export interface FooterLink {
  /** The visible link text. */
  label: ReactNode;
  /** The destination — a real `href` on a real `<a>`. */
  href: string;
}

export interface FooterColumn {
  /** Stable id — the React key and column identity (else the index). */
  id?: string;
  /** The column heading — renders as an `<h3>`. */
  title: ReactNode;
  /** The link group — each renders as an `<li>` with a real `<a href>`. */
  links: FooterLink[];
}

/* ─── props ───────────────────────────────────────────────────────────── */

export interface FooterProps extends ComponentPropsWithoutRef<"footer"> {
  /** Fine print / legal notes rendered above the directory. */
  footnotes?: ReactNode;

  /** Brand / logo / wordmark slot, rendered at the top-start of the band. */
  brand?: ReactNode;

  /** Short blurb under the brand (muted). */
  description?: ReactNode;

  /**
   * Ergonomic mode: the link-group columns, in order. Rendered FIRST; any
   * compound `<Footer.Column>` children fall through AFTER (the surfaces are
   * additive — no suppression).
   */
  columns?: FooterColumn[];

  /** Bottom-bar inline-start content — typically the copyright line. */
  copyright?: ReactNode;

  /** Bottom-bar legal links rendered after the copyright line. */
  legalLinks?: FooterLink[];

  /** Bottom-bar locale / region slot, rendered near the actions endcap. */
  locale?: ReactNode;

  /**
   * Bottom-bar inline-end content — a slot for social icons / secondary
   * links. Rendered as-is (the consumer supplies real anchors / icon buttons).
   */
  actions?: ReactNode;

  /**
   * Container width for the band body, from the `--zs-container-*` tokens.
   * Default `product`, matching dense product/legal page footers.
   */
  size?: ContainerSize;

  /**
   * Full-bleed band tone — the shared page-rhythm system. The root stamps
   * `data-tone`; the band treatment lives in `sections/_section-tone.css`.
   * - `default` (default): transparent; inherits the page backdrop.
   * - `muted`: a subtle full-bleed surface fill so the footer reads as its own
   *   panel — the common "the footer sits on a tinted slab" treatment.
   * - `accent`: an `--zs-accent` fill with the inner ink remapped to
   *   `--zs-accent-ink` — the bold contrast footer.
   */
  tone?: SectionTone;

  /** Compound `<Footer.Column>` parts (additive after `columns`). */
  children?: ReactNode;

  /**
   * Root `data-slot` value. Defaults to `"footer"`. A composing section can
   * override it so consumers target the outer element via its own slot
   * vocabulary. Mirrors Card / Hero / FeatureGrid / Container.
   */
  "data-slot"?: string;
}

/* ─── compound part props ─────────────────────────────────────────────── */

export interface FooterColumnProps {
  /** Stable id — the React key and column identity (else the index). */
  id?: string;
  /** The column heading — renders as an `<h3>`. */
  title: ReactNode;
  /**
   * The link group, supplied as the ergonomic `links` array. Each renders as
   * an `<li>` with a real `<a href>`. Mutually exclusive with `children`; when
   * both are present `links` wins.
   */
  links?: FooterLink[];
  /**
   * The link group as raw children (alternative to `links`) — the consumer
   * supplies their own `<a>` anchors, wrapped here in the column's `<ul>` so
   * the list semantics hold regardless of authoring mode.
   */
  children?: ReactNode;
}

/* ─── child walk ──────────────────────────────────────────────────────── */

/* Recursively flatten children, descending Fragments so Fragment-wrapped
 * compound parts (`<><Footer.Column/></>`) are visible to the normalizer.
 * Mirrors sections/FeatureGrid + sections/StatsBand + sections/Faq. */
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

/* ─── shared column renderer ──────────────────────────────────────────── */

/* The single source of truth for a link column — both the `columns` prop path
 * and the compound `<Footer.Column>` path funnel through here, so the two
 * surfaces render byte-identical columns. The title is a real `<h3>`; the
 * links are a real `<ul>` of `<li>` → `<a href>`. When the column comes from
 * the compound surface with raw `children` (rather than a `links` array), the
 * children are dropped into the `<ul>` so the list semantics still hold. */
interface ResolvedColumn {
  key: string;
  title: ReactNode;
  links?: FooterLink[];
  children?: ReactNode;
}

function renderColumn(column: ResolvedColumn) {
  const { title, links, children } = column;
  return (
    <div
      key={column.key}
      data-slot="footer-column"
      className="zs-footer__column"
    >
      <h3
        data-slot="footer-column-title"
        className="zs-footer__column-title"
      >
        {title}
      </h3>
      <ul data-slot="footer-links" className="zs-footer__links">
        {links != null
          ? links.map((link, index) => (
              <li
                key={link.href + ":" + index}
                className="zs-footer__link-item"
              >
                <a href={link.href} className="zs-footer__link">
                  {link.label}
                </a>
              </li>
            ))
          : children}
      </ul>
    </div>
  );
}

/* ─── root ────────────────────────────────────────────────────────────── */

const FooterRoot = forwardRef<HTMLElement, FooterProps>(function FooterRoot(
  {
    footnotes,
    brand,
    description,
    columns,
    copyright,
    legalLinks,
    locale,
    actions,
    size = "product",
    tone = "default",
    className,
    children,
    "data-slot": dataSlot = "footer",
    ...rest
  },
  ref,
) {
  const composedClassName = classnames("zs-footer", className);

  // ── Resolve the `columns` prop path ───────────────────────────────────
  // Keys are NAMESPACED by source surface (`prop:` / `compound:`) so a prop
  // column id and a compound column key can never collide.
  const propColumns: ResolvedColumn[] = (columns ?? []).map(
    (column, index) => ({
      key: `prop:${column.id ?? index}`,
      title: column.title,
      links: column.links,
    }),
  );

  // ── Resolve the compound `<Footer.Column>` path (ADDITIVE) ────────────
  // The link group comes from the `links` prop, else the part's raw children
  // (wrapped in the column's <ul> by renderColumn).
  const compoundColumns: ResolvedColumn[] = [];
  flattenChildren(children).forEach((child, index) => {
    if (!isValidElement(child) || child.type !== FooterColumn) return;
    const p = (child as ReactElement<FooterColumnProps>).props;
    compoundColumns.push({
      key: `compound:${(child as ReactElement).key ?? p.id ?? index}`,
      title: p.title,
      links: p.links,
      children: p.links == null ? p.children : undefined,
    });
  });

  const allColumns = [...propColumns, ...compoundColumns];
  const hasFootnotes = footnotes != null;
  const hasColumns = allColumns.length > 0;
  const hasBrandBlock = brand != null || description != null;
  const hasLegalLinks = (legalLinks?.length ?? 0) > 0;
  const hasBottom =
    copyright != null || hasLegalLinks || locale != null || actions != null;

  return (
    <footer
      {...rest}
      ref={ref as Ref<HTMLElement>}
      data-slot={dataSlot}
      data-section-band=""
      data-tone={tone}
      className={composedClassName}
    >
      <Container size={size} data-slot="footer-container">
        {hasFootnotes ? (
          <div data-slot="footer-footnotes" className="zs-footer__footnotes">
            {footnotes}
          </div>
        ) : null}

        {hasFootnotes && (hasBrandBlock || hasColumns) ? (
          <Separator
            className="zs-footer__separator"
            data-slot="footer-footnotes-separator"
          />
        ) : null}

        {/* Top row: the brand+blurb block beside the link-group columns. The
            Grid owns the collapse to stacked below --zs-bp-md (its responsive
            base count is 1; columns={{ md }} only promotes at ≥ the bp). */}
        {(hasBrandBlock || hasColumns) && (
          <div className="zs-footer__top">
            {hasBrandBlock ? (
              <div className="zs-footer__brand-block">
                {brand != null ? (
                  <div data-slot="footer-brand" className="zs-footer__brand">
                    {brand}
                  </div>
                ) : null}
                {description != null ? (
                  <p
                    data-slot="footer-description"
                    className="zs-footer__description"
                  >
                    {description}
                  </p>
                ) : null}
              </div>
            ) : null}

            {hasColumns ? (
              <Grid
                columns={{ md: Math.min(allColumns.length, 4) }}
                gap={6}
                data-slot="footer-columns"
                className="zs-footer__columns"
              >
                {allColumns.map((column) => renderColumn(column))}
              </Grid>
            ) : null}
          </div>
        )}

        {hasBottom ? (
          <>
            <Separator
              className="zs-footer__separator"
              data-slot="footer-separator"
            />
            <div data-slot="footer-bottom" className="zs-footer__bottom">
              <div
                data-slot="footer-bottom-start"
                className="zs-footer__bottom-start"
              >
                {copyright != null ? (
                  <div
                    data-slot="footer-copyright"
                    className="zs-footer__copyright"
                  >
                    {copyright}
                  </div>
                ) : null}
                {hasLegalLinks ? (
                  <ul
                    data-slot="footer-legal-links"
                    className="zs-footer__legal-links"
                  >
                    {legalLinks?.map((link, index) => (
                      <li
                        key={link.href + ":" + index}
                        className="zs-footer__legal-link-item"
                      >
                        <a href={link.href} className="zs-footer__legal-link">
                          {link.label}
                        </a>
                      </li>
                    ))}
                  </ul>
                ) : null}
              </div>
              {locale != null || actions != null ? (
                <div
                  data-slot="footer-bottom-end"
                  className="zs-footer__bottom-end"
                >
                  {locale != null ? (
                    <div
                      data-slot="footer-locale"
                      className="zs-footer__locale"
                    >
                      {locale}
                    </div>
                  ) : null}
                  {actions != null ? (
                    <div
                      data-slot="footer-actions"
                      className="zs-footer__actions"
                    >
                      {actions}
                    </div>
                  ) : null}
                </div>
              ) : null}
            </div>
          </>
        ) : null}
      </Container>
    </footer>
  );
});
FooterRoot.displayName = "Footer";

/* ─── compound part ───────────────────────────────────────────────────── */

/* Footer.Column is a MARKER: the root reads its props during the child walk
 * and renders the real column itself (via `renderColumn`) so the prop path and
 * the compound path produce identical markup. Rendering it standalone (outside
 * a `<Footer>`) is a misuse — it dev-warns and renders nothing rather than
 * emit an orphan column with no footer around it. */
function FooterColumn(_props: FooterColumnProps): ReactElement | null {
  if (process.env.NODE_ENV !== "production") {
    // eslint-disable-next-line no-console
    console.warn(
      "Footer.Column must be a direct (or Fragment-wrapped) child of " +
        "<Footer>; the root reads its props to render the link column. " +
        "Rendered standalone it produces nothing.",
    );
  }
  return null;
}
(FooterColumn as { displayName?: string }).displayName = "Footer.Column";

/* ─── public Footer namespace ─────────────────────────────────────────── */

type FooterComponent = typeof FooterRoot & {
  Column: typeof FooterColumn;
};

export const Footer = FooterRoot as FooterComponent;
Footer.Column = FooterColumn;
