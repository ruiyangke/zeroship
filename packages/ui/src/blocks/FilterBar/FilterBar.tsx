/*
 * FilterBar — a governed search/filter toolbar for collections (lists,
 * card grids, tables). It composes the real catalog primitives rather
 * than inventing new surface:
 *
 *   [ search Input (type="search", decorative Search Icon in startSlot) ]
 *   [ active-filter Tags (each `removable`, firing its own onRemove) ]
 *   [ "Clear" Button (variant="plain") — only when activeFilters present ]
 *   [ children — any extra inline controls ]
 *   [ ⇢ flex spacer ⇠ ]
 *   [ actions slot — trailing Button(s) ]
 *
 * Everything sits in a single `Cluster` so the row wraps onto multiple
 * lines as the container narrows; the spacer pushes `actions` to the
 * far (inline-end) edge on a single line and collapses gracefully when
 * wrapped.
 *
 * Composition (NO new surface paint of its own):
 *   - search:  `Input type="search"` with a decorative `Icon as={Search}`
 *     in its `startSlot`. The brief's adornment prop is the REAL Input
 *     prop name `startSlot` (not `prefix`/`adornment`).
 *   - chips:   `Tag removable` — the real Tag's trailing remove `<button
 *     aria-label="Remove …">`; FilterBar never builds its own remove UI.
 *   - clear / actions: real `Button`.
 *
 * a11y — role gating (the central governance decision):
 *   The root carries `role="search"` ONLY when the bar actually has a
 *   search input (i.e. `search` or `onSearchChange` was supplied). A
 *   chips-only / actions-only bar is NOT a search landmark, so we emit a
 *   plain `<div>` with no role — claiming `role="search"` without a
 *   search field would be a landmark lie. (We deliberately do NOT use
 *   `role="toolbar"`: that implies roving-tabindex arrow navigation,
 *   which this bar — a mix of a text field, chips, and buttons with
 *   native tab order — does not implement. Cluster is layout-only.)
 *
 *   The search Input is labelled: `searchLabel` if given, else the
 *   placeholder text, applied as `aria-label` (no visible label fits the
 *   inline toolbar form). Decorative icons are `aria-hidden` (the Icon
 *   primitive does this automatically when no `label` is passed). Each
 *   chip's remove button gets `aria-label="Remove {label}"` from Tag
 *   when the label is a string; when it is a non-string node, the
 *   consumer supplies `removeLabel` on the filter entry. Keyboard is
 *   native throughout (input + buttons + Tag's Backspace/Delete remove).
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import { Search } from "lucide-react";
import { classnames } from "../../components/_classnames";
import { Input } from "../../components/Input";
import { Tag } from "../../components/Tag";
import { Button } from "../../components/Button";
import { Icon } from "../../components/Icon";
import { Cluster } from "../../layouts/Cluster";

/** A single removable active-filter chip descriptor. */
export interface FilterBarActiveFilter {
  /** Stable React key / identity for the chip. */
  id: string;
  /** Visible chip label. */
  label: ReactNode;
  /** Fired when the chip's remove (×) button is activated. */
  onRemove?: () => void;
  /**
   * Accessible label for the remove button. Required when `label` is a
   * non-string node (Tag can only auto-derive `Remove {text}` from a
   * string). Forwarded to the real Tag's `removeLabel`.
   */
  removeLabel?: string;
}

export interface FilterBarProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Controlled search value. Supplying `search` OR `onSearchChange`
   * turns on the search Input (and the `role="search"` landmark). When
   * neither is given the search field is omitted entirely — FilterBar
   * becomes a chips + actions bar with no role.
   */
  search?: string;

  /** Fired with the next search string on every keystroke. */
  onSearchChange?: (value: string) => void;

  /** Search field placeholder. Default `"Search…"`. */
  searchPlaceholder?: string;

  /**
   * Accessible name for the search Input. Defaults to the resolved
   * placeholder text. Use this when the placeholder is decorative or
   * differs from how the field should be announced.
   */
  searchLabel?: string;

  /**
   * Active filters, rendered as removable `Tag` chips in order. Each
   * fires its own `onRemove`. An empty / absent list renders no chips
   * (and no Clear button).
   */
  activeFilters?: FilterBarActiveFilter[];

  /**
   * Clear-all handler. When set AND `activeFilters` is non-empty, a
   * trailing "Clear" `Button` (`variant="plain"`) is shown that fires
   * this. Omitted when there are no active filters (nothing to clear).
   */
  onClearFilters?: () => void;

  /** Visible text for the clear-all button. Default `"Clear"`. */
  clearLabel?: ReactNode;

  /** Trailing actions slot — e.g. a "Filters" / "New" Button. */
  actions?: ReactNode;

  /**
   * Extra inline controls placed between the chips/clear group and the
   * spacer (e.g. a sort `Select`). Wraps with the rest of the row.
   */
  children?: ReactNode;
}

/**
 * Governed search/filter toolbar for collections. Composes the real
 * Input + Tag + Button + Icon inside a wrapping Cluster. The root is a
 * `role="search"` landmark only when it owns a search field; otherwise
 * it is a plain, role-less group.
 */
export const FilterBar = forwardRef<HTMLDivElement, FilterBarProps>(
  function FilterBar(
    {
      search,
      onSearchChange,
      searchPlaceholder = "Search…",
      searchLabel,
      activeFilters,
      onClearFilters,
      clearLabel = "Clear",
      actions,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    // The search field — and with it the `role="search"` landmark —
    // exists only when the consumer wired up either side of the
    // controlled search contract. Presence, not truthiness: a
    // controlled `search=""` (empty box) still counts.
    const searchable = search !== undefined || onSearchChange !== undefined;

    const filters = activeFilters ?? [];
    const hasFilters = filters.length > 0;
    // Clear only when there is both a handler AND something to clear.
    const showClear = hasFilters && onClearFilters != null;

    // aria-label for the search field: explicit prop first, else the
    // placeholder (the inline toolbar form has no room for a visible
    // label, so the field always carries an accessible name).
    const resolvedSearchLabel = searchLabel ?? searchPlaceholder;

    return (
      <Cluster
        {...rest}
        ref={ref}
        // Landmark gating: a search role ONLY when we render a search
        // field. Reasserted after `...rest` so a caller can't strip it
        // when searchable, nor forge it when not. (Cluster forwards
        // unknown props to its <div>.)
        role={searchable ? "search" : undefined}
        data-slot="filter-bar"
        className={classnames("zs-filter-bar", className)}
      >
        {searchable ? (
          <Input
            type="search"
            data-slot="filter-bar-search"
            className="zs-filter-bar__search"
            value={search}
            onChange={(event) => onSearchChange?.(event.target.value)}
            placeholder={searchPlaceholder}
            aria-label={resolvedSearchLabel}
            startSlot={
              // Decorative — no `label`, so Icon emits aria-hidden.
              <Icon as={Search} size="sm" />
            }
          />
        ) : null}

        {hasFilters ? (
          <Cluster gap={1} data-slot="filter-bar-filters">
            {filters.map((filter) => (
              <Tag
                key={filter.id}
                removable
                removeLabel={filter.removeLabel}
                onRemove={filter.onRemove}
              >
                {filter.label}
              </Tag>
            ))}
          </Cluster>
        ) : null}

        {showClear ? (
          <Button
            type="button"
            variant="plain"
            size="small"
            data-slot="filter-bar-clear"
            onClick={() => onClearFilters?.()}
          >
            {clearLabel}
          </Button>
        ) : null}

        {children}

        {/* Flex spacer — pushes `actions` to the inline-end edge on a
            single line; harmless (zero-size) once the row wraps. */}
        <span className="zs-filter-bar__spacer" aria-hidden="true" />

        {actions != null ? (
          <Cluster gap={2} data-slot="filter-bar-actions">
            {actions}
          </Cluster>
        ) : null}
      </Cluster>
    );
  },
);
FilterBar.displayName = "FilterBar";
