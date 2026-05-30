/*
 * Breadcrumbs — a navigational breadcrumb trail.
 *
 * Dual surface (mirrors Card's ergonomic + compound shape):
 *
 *   1. Ergonomic `items` — the common case. Pass an array of
 *      `{ label, href?, current? }` and the trail builds itself:
 *
 *        <Breadcrumbs
 *          items={[
 *            { label: "Home", href: "/" },
 *            { label: "Projects", href: "/projects" },
 *            { label: "Acme", current: true },
 *          ]}
 *        />
 *
 *      A crumb with `href` renders a `Breadcrumbs.Link` (`<a>`); without
 *      `href` it renders as plain text. The crumb flagged `current` (or,
 *      if none is flagged, the LAST crumb) renders as `Breadcrumbs.Page`
 *      (`<span aria-current="page">`, NOT a link).
 *
 *   2. Compound parts — full control. Compose the parts yourself:
 *
 *        <Breadcrumbs>
 *          <Breadcrumbs.Item>
 *            <Breadcrumbs.Link href="/">Home</Breadcrumbs.Link>
 *          </Breadcrumbs.Item>
 *          <Breadcrumbs.Item>
 *            <Breadcrumbs.Link asChild>
 *              <RouterLink to="/projects">Projects</RouterLink>
 *            </Breadcrumbs.Link>
 *          </Breadcrumbs.Item>
 *          <Breadcrumbs.Item>
 *            <Breadcrumbs.Page>Acme</Breadcrumbs.Page>
 *          </Breadcrumbs.Item>
 *        </Breadcrumbs>
 *
 *      Separators are AUTO-INSERTED between adjacent `Breadcrumbs.Item`
 *      children (see the auto-insert note on the root) — consumers do not
 *      hand-roll them. An explicit `Breadcrumbs.Separator` is still
 *      exported for escape hatches but is rarely needed.
 *
 * `items` and `children` are MUTUALLY EXCLUSIVE — passing both dev-warns
 * and prefers `items` (children are dropped). This avoids an ambiguous
 * two-source trail.
 *
 * a11y: the root is `<nav aria-label="Breadcrumb"><ol>…</ol></nav>`. Each
 * crumb is an `<li>`. The current page is `aria-current="page"` and is
 * not a link. Separators are `<li role="presentation" aria-hidden="true">`
 * so AT skips them. Links get a `:focus-visible` ring via the
 * `--zs-focus-ring-*` tokens.
 *
 * Collapse (`maxItems`): when the crumb count exceeds `maxItems` the
 * middle collapses into a single ellipsis crumb — a real
 * `<button aria-expanded aria-label="Show N hidden breadcrumbs">…</button>`.
 * Activating it expands the full trail INLINE (internal state, no
 * portal/Menu dependency). The first crumb and the last `(maxItems - 1)`
 * crumbs always stay visible. A Menu-dropdown variant of the ellipsis is
 * a possible future enhancement; the inline expand keeps the dependency
 * surface flat. Collapse applies to the ergonomic `items` form only.
 */
import {
  Children,
  forwardRef,
  Fragment,
  isValidElement,
  useState,
  type ComponentPropsWithoutRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { Slot } from "../../components/_slot";
import { classnames } from "../../components/_classnames";

/* ─── public types ─────────────────────────────────────────────────────── */

export interface BreadcrumbItem {
  /** Crumb text (or any node). */
  label: ReactNode;
  /** Link target. Omitted → the crumb renders as plain text (non-link). */
  href?: string;
  /**
   * Marks the current page. The current crumb renders as
   * `Breadcrumbs.Page` (`<span aria-current="page">`), NOT a link. If no
   * item is flagged, the LAST item is treated as current.
   */
  current?: boolean;
}

export interface BreadcrumbsProps
  extends Omit<ComponentPropsWithoutRef<"nav">, "title"> {
  /**
   * Ergonomic trail. Map each entry to a crumb; the platform inserts
   * separators between them. Omit and use compound parts
   * (`Breadcrumbs.Item` / `.Link` / `.Page` / `.Separator`) for full
   * control. Mutually exclusive with `children` — passing both dev-warns
   * and prefers `items`.
   */
  items?: BreadcrumbItem[];
  /**
   * Separator rendered (aria-hidden) between crumbs. Default a chevron
   * `›`. Pass e.g. `"/"` for a slash trail, or any node.
   */
  separator?: ReactNode;
  /**
   * Collapse the middle of the trail into an ellipsis when the crumb
   * count exceeds this. The first crumb and the last `(maxItems - 1)`
   * crumbs stay visible; the rest collapse behind a real expand button.
   * Applies to the `items` form only.
   */
  maxItems?: number;
  /** Accessible label on the wrapping `<nav>`. Default `"Breadcrumb"`. */
  "aria-label"?: string;
  /**
   * Root `data-slot` value. Defaults to `"breadcrumbs"`. A composing
   * layout (e.g. `PageHeader.Breadcrumbs`) overrides it so consumers can
   * target the outer element via its own slot vocabulary.
   */
  "data-slot"?: string;
  /** Compound parts. Mutually exclusive with `items`. */
  children?: ReactNode;
}

export interface BreadcrumbsItemProps
  extends ComponentPropsWithoutRef<"li"> {
  children?: ReactNode;
}

export interface BreadcrumbsLinkProps
  extends ComponentPropsWithoutRef<"a"> {
  /**
   * Render-as the single child element rather than an `<a>` — pass a
   * router `<Link>` so client navigation works while keeping the
   * breadcrumb styling/semantics. Routed through `Slot` (React-19-safe
   * refs). Dev-warns when the child is not a single valid element.
   */
  asChild?: boolean;
  children?: ReactNode;
}

export interface BreadcrumbsPageProps
  extends ComponentPropsWithoutRef<"span"> {
  children?: ReactNode;
}

export interface BreadcrumbsSeparatorProps
  extends ComponentPropsWithoutRef<"li"> {
  /** Separator glyph/node. Falls back to the chevron default. */
  children?: ReactNode;
}

/** The default separator glyph — a single right-pointing angle quote. */
const DEFAULT_SEPARATOR = "›"; // ›

/* ─── compound parts ───────────────────────────────────────────────────── */

const BreadcrumbsItem = forwardRef<HTMLLIElement, BreadcrumbsItemProps>(
  function BreadcrumbsItem({ className, ...rest }, ref) {
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented contract attr via raw spread.
    return (
      <li
        {...rest}
        ref={ref}
        data-slot="breadcrumbs-item"
        className={classnames("zs-breadcrumbs__item", className)}
      />
    );
  },
);
BreadcrumbsItem.displayName = "Breadcrumbs.Item";

const BreadcrumbsLink = forwardRef<HTMLAnchorElement, BreadcrumbsLinkProps>(
  function BreadcrumbsLink(
    { asChild = false, className, children, ...rest },
    ref,
  ) {
    if (asChild) {
      if (!isValidElement(children)) {
        if (process.env.NODE_ENV !== "production") {
          // eslint-disable-next-line no-console
          console.warn(
            "Breadcrumbs.Link asChild expects a single React element child; received " +
              typeof children +
              "; rendering nothing.",
          );
        }
        return null;
      }
      // Route asChild through Slot so className composition + React-19
      // ref access behave identically to the default <a> path. Slot
      // composes the child ref internally, so we pass only the forwarded
      // ref.
      return (
        <Slot
          {...rest}
          ref={ref as Ref<unknown>}
          data-slot="breadcrumbs-link"
          className={classnames("zs-breadcrumbs__link", className)}
        >
          {children}
        </Slot>
      );
    }
    // Rest spread BEFORE internal data-slot so callers cannot overwrite
    // the documented contract attr.
    return (
      <a
        {...rest}
        ref={ref}
        data-slot="breadcrumbs-link"
        className={classnames("zs-breadcrumbs__link", className)}
      >
        {children}
      </a>
    );
  },
);
BreadcrumbsLink.displayName = "Breadcrumbs.Link";

const BreadcrumbsPage = forwardRef<HTMLSpanElement, BreadcrumbsPageProps>(
  function BreadcrumbsPage({ className, ...rest }, ref) {
    // The current page is `aria-current="page"` and NOT a link. Rest
    // spread BEFORE internal data-slot / aria-current so the contract
    // attrs cannot be desynced via raw spread.
    return (
      <span
        {...rest}
        ref={ref}
        data-slot="breadcrumbs-page"
        aria-current="page"
        className={classnames("zs-breadcrumbs__page", className)}
      />
    );
  },
);
BreadcrumbsPage.displayName = "Breadcrumbs.Page";

const BreadcrumbsSeparator = forwardRef<
  HTMLLIElement,
  BreadcrumbsSeparatorProps
>(function BreadcrumbsSeparator({ className, children, ...rest }, ref) {
  // Separators are decorative: role="presentation" + aria-hidden so AT
  // skips them entirely. Rest spread BEFORE the contract attrs.
  return (
    <li
      {...rest}
      ref={ref}
      data-slot="breadcrumbs-separator"
      role="presentation"
      aria-hidden="true"
      className={classnames("zs-breadcrumbs__separator", className)}
    >
      {children ?? DEFAULT_SEPARATOR}
    </li>
  );
});
BreadcrumbsSeparator.displayName = "Breadcrumbs.Separator";

/* ─── auto-insert separators between Item children ─────────────────────────
 *
 * In compound mode we auto-insert a `Breadcrumbs.Separator` between every
 * pair of adjacent `Breadcrumbs.Item` children so consumers never
 * hand-roll separators (decision: AUTO-INSERT between Items). We walk the
 * children, keep non-Item nodes (including any explicit Separator the
 * consumer DID pass) in place, and only inject a separator in the gap
 * between two Items when no element already sits between them. The type
 * check is `child.type === BreadcrumbsItem`, which is robust here because
 * the parts are module-private singletons — there is no fragile string /
 * displayName match.
 *
 * `Children.toArray` flattens nested ARRAYS but NOT `React.Fragment`s, so a
 * consumer's `<Breadcrumbs><>{items.map(...)}</></Breadcrumbs>` (the common
 * map/conditional shape) would arrive as ONE fragment child and get zero
 * separators. We recursively flatten Fragment children FIRST — splicing in
 * their `props.children` (recursing for nested fragments) — so the
 * separator walk sees the real `Breadcrumbs.Item` run.
 */
function flattenFragments(children: ReactNode): ReactNode[] {
  const out: ReactNode[] = [];
  for (const child of Children.toArray(children)) {
    if (
      isValidElement(child) &&
      (child as ReactElement).type === Fragment
    ) {
      // Recurse into the Fragment's children (handles nested fragments).
      out.push(
        ...flattenFragments(
          (child as ReactElement<{ children?: ReactNode }>).props.children,
        ),
      );
    } else {
      out.push(child);
    }
  }
  return out;
}

function withAutoSeparators(
  children: ReactNode,
  separator: ReactNode,
): ReactNode {
  const array = flattenFragments(children);
  const out: ReactNode[] = [];
  let lastWasItem = false;
  let sepKey = 0;
  for (const child of array) {
    const isItem =
      isValidElement(child) &&
      (child as ReactElement).type === BreadcrumbsItem;
    if (isItem && lastWasItem) {
      out.push(
        <BreadcrumbsSeparator key={`__auto-sep-${sepKey++}`}>
          {separator}
        </BreadcrumbsSeparator>,
      );
    }
    out.push(child);
    // Only Item children gate the next auto-separator. A consumer-placed
    // node (e.g. an explicit Separator) resets the run so we don't double
    // up around it.
    lastWasItem = isItem;
  }
  return out;
}

/* ─── ergonomic items → crumb <li>s ────────────────────────────────────── */

function renderItemCrumb(
  item: BreadcrumbItem,
  isCurrent: boolean,
  key: string,
): ReactElement {
  let inner: ReactNode;
  if (isCurrent) {
    inner = <BreadcrumbsPage>{item.label}</BreadcrumbsPage>;
  } else if (item.href != null) {
    inner = <BreadcrumbsLink href={item.href}>{item.label}</BreadcrumbsLink>;
  } else {
    // No href and not current → plain non-link text. Wrap in a span so
    // the crumb has a styleable hook distinct from a link.
    inner = (
      <span data-slot="breadcrumbs-text" className="zs-breadcrumbs__text">
        {item.label}
      </span>
    );
  }
  return <BreadcrumbsItem key={key}>{inner}</BreadcrumbsItem>;
}

/* ─── ellipsis (collapse) button — inline expand, no portal ────────────── */

function EllipsisCrumb({
  hiddenCount,
  expanded,
  onExpand,
  testId,
}: {
  hiddenCount: number;
  expanded: boolean;
  onExpand: () => void;
  testId?: string;
}) {
  return (
    <li
      data-slot="breadcrumbs-item"
      className="zs-breadcrumbs__item zs-breadcrumbs__item--ellipsis"
    >
      <button
        type="button"
        data-slot="breadcrumbs-ellipsis"
        data-testid={testId}
        className="zs-breadcrumbs__ellipsis"
        aria-expanded={expanded}
        aria-label={`Show ${hiddenCount} hidden breadcrumbs`}
        onClick={onExpand}
      >
        <span aria-hidden="true" className="zs-breadcrumbs__ellipsis-glyph">
          {"…"}
        </span>
      </button>
    </li>
  );
}

/* ─── root ─────────────────────────────────────────────────────────────── */

const BreadcrumbsRoot = forwardRef<HTMLElement, BreadcrumbsProps>(
  function BreadcrumbsRoot(
    {
      items,
      separator = DEFAULT_SEPARATOR,
      maxItems,
      className,
      children,
      "aria-label": ariaLabel = "Breadcrumb",
      "data-slot": dataSlot = "breadcrumbs",
      ...rest
    },
    ref,
  ) {
    // Inline-expand state for the collapsed (maxItems) trail. Begins
    // collapsed; activating the ellipsis button flips it true and the
    // full trail re-renders in place.
    const [expanded, setExpanded] = useState(false);

    const hasItems = Array.isArray(items);
    const hasChildren =
      children != null && Children.count(children) > 0;

    if (process.env.NODE_ENV !== "production" && hasItems && hasChildren) {
      // eslint-disable-next-line no-console
      console.warn(
        "Breadcrumbs received both `items` and `children`. These are " +
          "mutually exclusive — pass `items` for the ergonomic trail OR " +
          "compound parts as children, not both. Using `items` and " +
          "ignoring children.",
      );
    }

    let body: ReactNode;

    if (hasItems) {
      const list = items as BreadcrumbItem[];
      // Resolve which crumb is current: the explicitly flagged one, else
      // the last crumb. (If several are flagged, the FIRST flagged wins
      // and the rest render as their href/text shape.)
      const flaggedIndex = list.findIndex((it) => it.current === true);
      const currentIndex = flaggedIndex >= 0 ? flaggedIndex : list.length - 1;

      // Decide the visible set + whether to collapse. The head is always
      // the first crumb; the tail is the last `(maxItems - 1)` crumbs. We
      // compute the hidden span FIRST and only collapse when it is
      // non-empty — a maxItems that would hide nothing (e.g. maxItems=1 on
      // a 2-crumb trail, where head+tail already cover everything) must NOT
      // render an ellipsis that hides nothing. The current crumb is ALWAYS
      // kept visible: if its resolved index falls in the hidden middle we
      // extend the tail to start no later than it.
      const headCount = 1;
      const baseTailStart = list.length - Math.max(maxItems != null ? maxItems - 1 : 0, 1);
      // Keep the current crumb visible by pulling the tail start back to it.
      const tailStart = Math.min(baseTailStart, currentIndex);
      const hiddenCount = Math.max(tailStart - headCount, 0);
      const shouldCollapse =
        !expanded &&
        typeof maxItems === "number" &&
        maxItems >= 1 &&
        list.length > maxItems &&
        hiddenCount > 0;

      if (shouldCollapse) {
        const head = list.slice(0, headCount);
        const tail = list.slice(tailStart);

        const crumbs: ReactElement[] = [];
        head.forEach((it, i) => {
          crumbs.push(renderItemCrumb(it, i === currentIndex, `crumb-${i}`));
        });
        crumbs.push(
          <EllipsisCrumb
            key="__ellipsis"
            hiddenCount={hiddenCount}
            expanded={false}
            onExpand={() => setExpanded(true)}
            testId="breadcrumbs-ellipsis"
          />,
        );
        tail.forEach((it, i) => {
          const absIndex = tailStart + i;
          crumbs.push(
            renderItemCrumb(it, absIndex === currentIndex, `crumb-${absIndex}`),
          );
        });
        body = interleaveSeparators(crumbs, separator);
      } else {
        const crumbs = list.map((it, i) =>
          renderItemCrumb(it, i === currentIndex, `crumb-${i}`),
        );
        body = interleaveSeparators(crumbs, separator);
      }
    } else {
      // Compound mode — auto-insert separators between Item children.
      body = withAutoSeparators(children, separator);
    }

    return (
      <nav
        {...rest}
        ref={ref as Ref<HTMLElement>}
        data-slot={dataSlot}
        aria-label={ariaLabel}
        className={classnames("zs-breadcrumbs", className)}
      >
        <ol className="zs-breadcrumbs__list">{body}</ol>
      </nav>
    );
  },
);
BreadcrumbsRoot.displayName = "Breadcrumbs";

/* Interleave separator <li>s between an array of already-built crumb
 * <li>s (the ergonomic path; compound mode uses withAutoSeparators). */
function interleaveSeparators(
  crumbs: ReactElement[],
  separator: ReactNode,
): ReactNode[] {
  const out: ReactNode[] = [];
  crumbs.forEach((crumb, i) => {
    if (i > 0) {
      out.push(
        <BreadcrumbsSeparator key={`sep-${i}`}>
          {separator}
        </BreadcrumbsSeparator>,
      );
    }
    out.push(crumb);
  });
  return out;
}

/* ─── public namespace ─────────────────────────────────────────────────── */

type BreadcrumbsComponent = typeof BreadcrumbsRoot & {
  Item: typeof BreadcrumbsItem;
  Link: typeof BreadcrumbsLink;
  Page: typeof BreadcrumbsPage;
  Separator: typeof BreadcrumbsSeparator;
};

export const Breadcrumbs = BreadcrumbsRoot as BreadcrumbsComponent;
Breadcrumbs.Item = BreadcrumbsItem;
Breadcrumbs.Link = BreadcrumbsLink;
Breadcrumbs.Page = BreadcrumbsPage;
Breadcrumbs.Separator = BreadcrumbsSeparator;
