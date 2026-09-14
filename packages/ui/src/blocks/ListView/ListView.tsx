/*
 * ListView — a governed stacked list of item rows.
 *
 * The canonical "rows of records" surface: a vertical list where each
 * row is a horizontal lockup of
 *
 *   [leading]  [ title / description ]  [ meta ]  [ trailing ]
 *
 * leading  — an Avatar / Icon (decorative unless it carries meaning).
 * content  — a Stack column of a title and an optional description.
 * meta      — secondary trailing-aligned text (e.g. a timestamp).
 * trailing — actions / a Badge / a chevron / a kebab menu.
 *
 * Generalizes the row patterns the builder hand-rolls (ledger rows,
 * member lists, notification feeds) into one composing block that paints
 * the dividers, governs the padding, and — crucially — gets the
 * interactive-row accessibility right.
 *
 * Like Card and EmptyState, ListView exposes a DUAL surface:
 *
 *   1. Ergonomic `items` — the 80% case as data:
 *        <ListView
 *          items={[
 *            { id: "1", leading: <Avatar … />, title: "Ada", description: "…",
 *              meta: "2h", trailing: <Icon as={ChevronRight} />, href: "/u/ada" },
 *          ]}
 *        />
 *
 *   2. Compound parts — full control over composition:
 *        <ListView>
 *          <ListView.Item>
 *            <ListView.Leading><Avatar … /></ListView.Leading>
 *            <ListView.Content>
 *              <ListView.Title>Ada</ListView.Title>
 *              <ListView.Description>…</ListView.Description>
 *            </ListView.Content>
 *            <ListView.Trailing><Badge>New</Badge></ListView.Trailing>
 *          </ListView.Item>
 *        </ListView>
 *
 * `items` and `children` are mutually exclusive — supplying both is a
 * dev-warn and `items` wins (the data path is the governed one).
 *
 * ─── Interactive-row accessibility (the crux) ──────────────────────────
 *
 * A whole-row link MUST NOT swallow the trailing controls. Mirroring the
 * Card discipline (anti-pattern #2 from the slice-3 survey — never wrap
 * interactive content inside another interactive), the ergonomic path
 * wraps ONLY the "main" area (leading + title + description) in the row's
 * `<a href>` / `<button>`. The `trailing` slot — which is where the
 * actions, the kebab menu, and clickable Badges live — stays a SIBLING,
 * OUTSIDE the row link. So a row reads to AT as:
 *
 *   <li>
 *     <a href="…">  leading · title · description  </a>   ← the row link
 *     <div data-slot="list-view-trailing"> <Button …/> </div>  ← separate
 *   </li>
 *
 * Two independent tab stops, no nested interactives, no "button inside a
 * link" invalid HTML. When neither `href` nor `onClick` is set the main
 * area is plain text (a `<div>`), and the whole item is non-interactive.
 *
 * The compound path does NOT auto-wrap — the consumer composes their own
 * `<a>`/`<button>` inside `ListView.Content` if they want an interactive
 * row, keeping the trailing controls in a sibling `ListView.Trailing`.
 *
 * `data-slot="list-view"` (and `list-view-<part>` on each subpart)
 * mirrors the Card / EmptyState data-slot vocabulary so consumers can
 * target parts in CSS without leaning on the internal BEM class names.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type MouseEventHandler,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";

export type ListViewDensity = "comfortable" | "compact";

export interface ListViewItem {
  /** Stable key for the row. */
  id: string;
  /**
   * Leading visual — typically an `<Avatar>` or an `<Icon>`. Decorative
   * by default (the title carries the meaning); pass a `label` to the
   * Icon / an `alt` to the Avatar only when the glyph is the sole
   * carrier of meaning.
   */
  leading?: ReactNode;
  /** Primary line. Rendered inside the row link when the row is interactive. */
  title: ReactNode;
  /** Optional secondary line beneath the title (muted). */
  description?: ReactNode;
  /**
   * Secondary trailing-aligned text — e.g. a timestamp or a count.
   * Rendered between the content column and the `trailing` slot, and
   * (like trailing) kept OUTSIDE the row link so it doesn't bloat the
   * link's accessible name.
   */
  meta?: ReactNode;
  /**
   * Trailing controls — actions, a `<Badge>`, a chevron `<Icon>`, a
   * kebab menu. ALWAYS rendered OUTSIDE the row link so an interactive
   * row never nests interactives. A trailing `<Button>` is its own tab
   * stop and fires without activating the row.
   */
  trailing?: ReactNode;
  /**
   * Whole-row link target. When set, the main area (leading + title +
   * description) is wrapped in an `<a href>`; `trailing`/`meta` stay
   * outside it. Prefer `href` over `onClick` for navigation — it gives
   * real anchor semantics (open-in-new-tab, copy-link, etc).
   */
  href?: string;
  /**
   * Interactive row without navigation. When set (and `href` is not),
   * the main area is wrapped in a `<button type="button">`. Ignored when
   * `href` is present (a link can't also be a button).
   */
  onClick?: MouseEventHandler<HTMLButtonElement>;
}

export interface ListViewProps extends ComponentPropsWithoutRef<"ul"> {
  /**
   * Row data — the ergonomic surface. Mutually exclusive with
   * `children`; if both are passed, `items` wins and a dev-warn fires.
   */
  items?: ListViewItem[];
  /**
   * Hairline divider between rows. Default `true`. Set `false` for a
   * flush, borderless list.
   */
  divided?: boolean;
  /**
   * Row padding density. `comfortable` (default) is the roomy listing
   * default; `compact` tightens the block padding for dense tables of
   * rows. Affects padding only — never type size or gap semantics.
   */
  density?: ListViewDensity;
  /**
   * Compound parts (`ListView.Item` …). Mutually exclusive with `items`.
   */
  children?: ReactNode;
  /**
   * Root `data-slot` value. Defaults to `"list-view"`. A composing block
   * can override it to expose its own slot vocabulary. Mirrors Card.
   */
  "data-slot"?: string;
}

/* ─── render helper: one ergonomic item → <li> ─────────────────────────── */

function renderItem(item: ListViewItem): ReactNode {
  const { id, leading, title, description, meta, trailing, href, onClick } =
    item;

  // The "main" area: leading + content column. This is the ONLY thing
  // that ever goes inside the row link, so trailing/meta interactives
  // are never nested inside an <a>/<button> (the a11y crux).
  const main = (
    <>
      {leading != null ? (
        <span data-slot="list-view-leading" className="zs-list-view__leading">
          {leading}
        </span>
      ) : null}
      <span data-slot="list-view-content" className="zs-list-view__content">
        <span data-slot="list-view-title" className="zs-list-view__title">
          {title}
        </span>
        {description != null ? (
          <span
            data-slot="list-view-description"
            className="zs-list-view__description"
          >
            {description}
          </span>
        ) : null}
      </span>
    </>
  );

  // Interactivity precedence: href → <a> (real anchor semantics), else
  // onClick → <button>, else a plain <div> (non-interactive). Either
  // interactive wrapper carries data-interactive so CSS can paint the
  // hover/focus affordance on the link, not the whole <li>.
  let mainEl: ReactNode;
  if (href != null) {
    mainEl = (
      <a
        href={href}
        data-slot="list-view-main"
        data-interactive=""
        className="zs-list-view__main"
      >
        {main}
      </a>
    );
  } else if (onClick != null) {
    mainEl = (
      <button
        type="button"
        onClick={onClick}
        data-slot="list-view-main"
        data-interactive=""
        className="zs-list-view__main"
      >
        {main}
      </button>
    );
  } else {
    mainEl = (
      <div data-slot="list-view-main" className="zs-list-view__main">
        {main}
      </div>
    );
  }

  // meta + trailing live in an aside cluster, OUTSIDE mainEl, so they are
  // separate tab stops and never inside the row link.
  const aside =
    meta != null || trailing != null ? (
      <span data-slot="list-view-aside" className="zs-list-view__aside">
        {meta != null ? (
          <span data-slot="list-view-meta" className="zs-list-view__meta">
            {meta}
          </span>
        ) : null}
        {trailing != null ? (
          <span
            data-slot="list-view-trailing"
            className="zs-list-view__trailing"
          >
            {trailing}
          </span>
        ) : null}
      </span>
    ) : null;

  return (
    <li key={id} data-slot="list-view-item" className="zs-list-view__item">
      {mainEl}
      {aside}
    </li>
  );
}

/* ─── ListView root ────────────────────────────────────────────────────── */

const ListViewRoot = forwardRef<HTMLUListElement, ListViewProps>(
  function ListViewRoot(
    {
      items,
      divided = true,
      density = "comfortable",
      className,
      children,
      "data-slot": dataSlot = "list-view",
      ...rest
    },
    ref,
  ) {
    // Dev-mode validation: items + children are mutually exclusive. The
    // data path is the governed one, so items wins; warn so the ignored
    // children are diagnosable. DCEs out of production builds.
    if (process.env.NODE_ENV !== "production") {
      if (items != null && children != null) {
        // eslint-disable-next-line no-console
        console.warn(
          "ListView received both `items` and `children`; they are mutually " +
            "exclusive. Rendering `items` and ignoring `children`.",
        );
      }
      // `item.id` is the documented stable React key. Duplicate ids
      // silently break reconciliation (rows reuse the wrong DOM / state),
      // so warn — once per render — naming each offending id. DCEs out of
      // production builds.
      if (items != null) {
        const seen = new Set<string>();
        for (const item of items) {
          if (seen.has(item.id)) {
            // eslint-disable-next-line no-console
            console.warn(
              "ListView received duplicate `item.id` " +
                JSON.stringify(item.id) +
                ". Ids are the stable React keys and must be unique; " +
                "duplicates break row reconciliation.",
            );
          } else {
            seen.add(item.id);
          }
        }
      }
    }

    const composedClassName = classnames("zs-list-view", className);

    return (
      <ul
        {...rest}
        ref={ref}
        data-slot={dataSlot}
        data-density={density}
        data-divided={divided ? "" : undefined}
        className={composedClassName}
      >
        {items != null ? items.map(renderItem) : children}
      </ul>
    );
  },
);
ListViewRoot.displayName = "ListView";

/* ─── Subparts (compound surface) ──────────────────────────────────────── */

type SpanProps = ComponentPropsWithoutRef<"span">;
type ListItemProps = ComponentPropsWithoutRef<"li">;

export type ListViewLeadingProps = SpanProps;
export type ListViewContentProps = SpanProps;
export type ListViewTitleProps = SpanProps;
export type ListViewDescriptionProps = SpanProps;
export type ListViewMetaProps = SpanProps;
export type ListViewTrailingProps = SpanProps;

const ListViewItemPart = forwardRef<HTMLLIElement, ListItemProps>(
  function ListViewItem({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot so callers can't overwrite
    // the documented contract attr via raw spread (Card 🟢 6 convention).
    return (
      <li
        {...rest}
        ref={ref}
        data-slot="list-view-item"
        className={classnames("zs-list-view__item", className)}
      />
    );
  },
);
ListViewItemPart.displayName = "ListView.Item";

const ListViewLeading = forwardRef<HTMLSpanElement, ListViewLeadingProps>(
  function ListViewLeading({ className, ...rest }, ref) {
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="list-view-leading"
        className={classnames("zs-list-view__leading", className)}
      />
    );
  },
);
ListViewLeading.displayName = "ListView.Leading";

const ListViewContent = forwardRef<HTMLSpanElement, ListViewContentProps>(
  function ListViewContent({ className, ...rest }, ref) {
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="list-view-content"
        className={classnames("zs-list-view__content", className)}
      />
    );
  },
);
ListViewContent.displayName = "ListView.Content";

const ListViewTitle = forwardRef<HTMLSpanElement, ListViewTitleProps>(
  function ListViewTitle({ className, ...rest }, ref) {
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="list-view-title"
        className={classnames("zs-list-view__title", className)}
      />
    );
  },
);
ListViewTitle.displayName = "ListView.Title";

const ListViewDescription = forwardRef<
  HTMLSpanElement,
  ListViewDescriptionProps
>(function ListViewDescription({ className, ...rest }, ref) {
  return (
    <span
      {...rest}
      ref={ref}
      data-slot="list-view-description"
      className={classnames("zs-list-view__description", className)}
    />
  );
});
ListViewDescription.displayName = "ListView.Description";

const ListViewMeta = forwardRef<HTMLSpanElement, ListViewMetaProps>(
  function ListViewMeta({ className, ...rest }, ref) {
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="list-view-meta"
        className={classnames("zs-list-view__meta", className)}
      />
    );
  },
);
ListViewMeta.displayName = "ListView.Meta";

const ListViewTrailing = forwardRef<HTMLSpanElement, ListViewTrailingProps>(
  function ListViewTrailing({ className, ...rest }, ref) {
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="list-view-trailing"
        className={classnames("zs-list-view__trailing", className)}
      />
    );
  },
);
ListViewTrailing.displayName = "ListView.Trailing";

/* ─── public ListView namespace ────────────────────────────────────────── */

type ListViewComponent = typeof ListViewRoot & {
  Item: typeof ListViewItemPart;
  Leading: typeof ListViewLeading;
  Content: typeof ListViewContent;
  Title: typeof ListViewTitle;
  Description: typeof ListViewDescription;
  Meta: typeof ListViewMeta;
  Trailing: typeof ListViewTrailing;
};

export const ListView = ListViewRoot as ListViewComponent;
ListView.Item = ListViewItemPart;
ListView.Leading = ListViewLeading;
ListView.Content = ListViewContent;
ListView.Title = ListViewTitle;
ListView.Description = ListViewDescription;
ListView.Meta = ListViewMeta;
ListView.Trailing = ListViewTrailing;
