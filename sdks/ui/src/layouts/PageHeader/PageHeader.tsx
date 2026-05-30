/*
 * PageHeader — the page title band composition.
 *
 * Canonical structure (this is the REQUIRED shape — `PageHeader.Text` is
 * the `Stack` that forms the start-side text column; Breadcrumbs / Title /
 * Description go INSIDE it):
 *
 *   <PageHeader>
 *     <PageHeader.Text>
 *       <PageHeader.Breadcrumbs>        {/* optional *​/}
 *         <li><a href="/">Home</a></li>
 *         <li aria-hidden="true">/</li>
 *         <li><a href="/projects">Projects</a></li>
 *       </PageHeader.Breadcrumbs>
 *       <PageHeader.Title>Acme dashboard</PageHeader.Title>
 *       <PageHeader.Description>            {/* optional *​/}
 *         Overview of your workspace.
 *       </PageHeader.Description>
 *     </PageHeader.Text>
 *     <PageHeader.Actions>                  {/* optional *​/}
 *       <Button variant="gray">Export</Button>
 *       <Button>New project</Button>
 *     </PageHeader.Actions>
 *   </PageHeader>
 *
 * `PageHeader.Text` is REQUIRED to form the text column; only
 * `.Breadcrumbs`, `.Description`, and `.Actions` are optional.
 *
 * Layout — a row that wraps gracefully on narrow widths:
 *   - a START text column stacking Breadcrumbs → Title → Description
 *     (composes the `Stack` primitive), and
 *   - an END actions group (composes the `Cluster` primitive, right-
 *     aligned, wraps).
 * The two regions sit in a wrapping row with `justify-content:
 * space-between`; when the row is too narrow they wrap so Actions drop
 * below the text column instead of crushing it.
 *
 * a11y / semantics:
 *   - `.Title` is an `<h1>` by default — the page's primary heading.
 *     `asChild` reLEVELS it (e.g. to `<h2>`) so the heading matches the
 *     surrounding document outline when the band is not the top of the
 *     page.
 *   - `.Breadcrumbs` is a `<nav aria-label="Breadcrumb">` wrapping an
 *     ordered list (`<ol>`); consumers place `<li>` items with `<a>`
 *     links and any visual separators inside.
 *   - `.Description` is a muted `<p>`.
 *   - `.Actions` is a right-aligned `Cluster` (no role — a layout group
 *     of buttons/links).
 *
 * The ROOT supports `asChild`: PageHeader is a container that renders
 * whatever parts/children the consumer passes (it has no fixed composed
 * body the way AppShell does), so a Slot that render-as-es the root
 * element while keeping the children in place is safe. Routed through
 * `Slot` with a dev-warn (mirrors Card root). `.Title` likewise supports
 * `asChild` + dev-warn for the heading relevel.
 *
 * Composes Wave-1 primitives (`Stack`, `Cluster`) — it does not re-roll
 * flex rows.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";
import { Stack } from "../Stack";
import { Cluster } from "../Cluster";

/* ─── props ───────────────────────────────────────────────────────────── */

export interface PageHeaderProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Render-as the single child element rather than a `<div>`. The
   * PageHeader root is a plain container (no fixed composed body), so the
   * children keep their place inside the render-as target. Routed
   * through `Slot` (React-19-safe refs).
   */
  asChild?: boolean;
}

export type PageHeaderBreadcrumbsProps = ComponentPropsWithoutRef<"nav"> & {
  /**
   * Accessible label for the breadcrumb landmark. Defaults to
   * `"Breadcrumb"` (the conventional value). Localized consumers
   * override.
   */
  label?: string;
};
export interface PageHeaderTitleProps
  extends ComponentPropsWithoutRef<"h1"> {
  /**
   * Render-as the single child element rather than an `<h1>` — use to
   * relevel the heading (e.g. `<h2>`) so it matches the surrounding
   * document outline. Routed through `Slot`.
   */
  asChild?: boolean;
}
export type PageHeaderDescriptionProps = ComponentPropsWithoutRef<"p">;
export type PageHeaderActionsProps = ComponentPropsWithoutRef<"div">;

/* ─── root ────────────────────────────────────────────────────────────── */

const PageHeaderRoot = forwardRef<HTMLDivElement, PageHeaderProps>(
  function PageHeaderRoot(
    { asChild = false, className, children, ...rest },
    ref,
  ) {
    // Dev-mode parity with Card: warn when asChild has no single valid
    // element child (Slot would render nothing silently). DCEs in prod.
    if (process.env.NODE_ENV !== "production") {
      if (asChild && !isValidElement(children)) {
        // eslint-disable-next-line no-console
        console.warn(
          "PageHeader asChild requires a single React element child; rendering nothing.",
        );
      }
    }
    const Comp = asChild ? Slot : "div";
    return (
      <Comp
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        data-slot="page-header"
        className={classnames("zs-page-header", className)}
      >
        {children}
      </Comp>
    );
  },
);
PageHeaderRoot.displayName = "PageHeader";

/* ─── Breadcrumbs — <nav aria-label="Breadcrumb"> wrapping an <ol> ────── */

const PageHeaderBreadcrumbs = forwardRef<
  HTMLElement,
  PageHeaderBreadcrumbsProps
>(function PageHeaderBreadcrumbs(
  { label = "Breadcrumb", className, children, ...rest },
  ref,
) {
  // Rest spread BEFORE internal data-slot / aria-label so callers cannot
  // desync the contract attrs via raw spread; the documented surface for
  // the label is the `label` prop.
  return (
    <nav
      {...rest}
      ref={ref as Ref<HTMLElement>}
      data-slot="page-header-breadcrumbs"
      aria-label={label}
      className={classnames("zs-page-header__breadcrumbs", className)}
    >
      <ol className="zs-page-header__breadcrumbs-list">{children}</ol>
    </nav>
  );
});
PageHeaderBreadcrumbs.displayName = "PageHeader.Breadcrumbs";

/* ─── Title — <h1> by default; asChild relevels ───────────────────────── */

const PageHeaderTitle = forwardRef<HTMLHeadingElement, PageHeaderTitleProps>(
  function PageHeaderTitle(
    { asChild = false, className, children, ...rest },
    ref,
  ) {
    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.warn(
            "PageHeader.Title asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      // Route asChild through Slot so className composition + React-19
      // ref access behave identically to the default path. Slot composes
      // the child ref internally, so we pass only the forwarded ref.
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          data-slot="page-header-title"
          className={classnames("zs-page-header__title", className)}
        >
          {children}
        </Slot>
      );
    }
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented contract attr.
    return (
      <h1
        {...rest}
        ref={ref}
        data-slot="page-header-title"
        className={classnames("zs-page-header__title", className)}
      >
        {children}
      </h1>
    );
  },
);
PageHeaderTitle.displayName = "PageHeader.Title";

/* ─── Description — muted <p> ─────────────────────────────────────────── */

const PageHeaderDescription = forwardRef<
  HTMLParagraphElement,
  PageHeaderDescriptionProps
>(function PageHeaderDescription({ className, ...rest }, ref) {
  return (
    <p
      {...rest}
      ref={ref}
      data-slot="page-header-description"
      className={classnames("zs-page-header__description", className)}
    />
  );
});
PageHeaderDescription.displayName = "PageHeader.Description";

/* ─── Actions — right-aligned Cluster ─────────────────────────────────── */

const PageHeaderActions = forwardRef<HTMLDivElement, PageHeaderActionsProps>(
  function PageHeaderActions({ className, children, ...rest }, ref) {
    // Compose the Cluster primitive directly (wrapping inline group,
    // justified to the end). Cluster owns the layout; we pass the semantic
    // `data-slot="page-header-actions"` (Cluster honors a consumer
    // data-slot, defaulting to `"cluster"`) so the composed part owns its
    // slot vocabulary, and add `zs-page-header__actions` via Cluster's
    // `classnames` composition so the band's CSS + consumers can target
    // the actions group.
    return (
      <Cluster
        {...rest}
        ref={ref}
        justify="end"
        data-slot="page-header-actions"
        className={classnames("zs-page-header__actions", className)}
      >
        {children}
      </Cluster>
    );
  },
);
PageHeaderActions.displayName = "PageHeader.Actions";

/* ─── Text column — stacks Breadcrumbs / Title / Description ──────────────
 *
 * REQUIRED: `PageHeader.Text` is the start-side `Stack` column that holds
 * Breadcrumbs / Title / Description. They MUST be nested inside it so the
 * band reads as text-column + actions; placing them as bare root children
 * does not form the column. */
export type PageHeaderTextProps = ComponentPropsWithoutRef<"div">;

const PageHeaderText = forwardRef<HTMLDivElement, PageHeaderTextProps>(
  function PageHeaderText({ className, children, ...rest }, ref) {
    // Compose the Stack primitive directly (the start-side text column).
    // Stack owns the layout; we pass the semantic
    // `data-slot="page-header-text"` (Stack honors a consumer data-slot,
    // defaulting to `"stack"`) so the composed part owns its slot
    // vocabulary, and add `zs-page-header__text` via Stack's `classnames`
    // composition.
    return (
      <Stack
        {...rest}
        ref={ref}
        gap={1}
        data-slot="page-header-text"
        className={classnames("zs-page-header__text", className)}
      >
        {children}
      </Stack>
    );
  },
);
PageHeaderText.displayName = "PageHeader.Text";

/* ─── public namespace ────────────────────────────────────────────────── */

type PageHeaderComponent = typeof PageHeaderRoot & {
  Breadcrumbs: typeof PageHeaderBreadcrumbs;
  Title: typeof PageHeaderTitle;
  Description: typeof PageHeaderDescription;
  Actions: typeof PageHeaderActions;
  Text: typeof PageHeaderText;
};

export const PageHeader = PageHeaderRoot as PageHeaderComponent;
PageHeader.Breadcrumbs = PageHeaderBreadcrumbs;
PageHeader.Title = PageHeaderTitle;
PageHeader.Description = PageHeaderDescription;
PageHeader.Actions = PageHeaderActions;
PageHeader.Text = PageHeaderText;
