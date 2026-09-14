/*
 * DescriptionList — a semantic key→value list.
 *
 * The canonical "details panel" surface: a column of term/detail pairs
 * (Status: Active, Plan: Pro, Created: …). Built on the native `<dl>`
 * grouping so assistive tech announces it as a description list and pairs
 * each term with its detail — NOT a re-rolled grid of divs.
 *
 *   <DescriptionList>
 *     <DescriptionList.Item>
 *       <DescriptionList.Term>Status</DescriptionList.Term>
 *       <DescriptionList.Detail>Active</DescriptionList.Detail>
 *     </DescriptionList.Item>
 *     …
 *   </DescriptionList>
 *
 * Structure: the root is a `<dl>`. Each `.Item` is a `<div>` — a valid
 * grouping wrapper inside `<dl>` per the HTML spec (a `<div>` may group a
 * `<dt>`/`<dd>` pair), which lets us style and divide per-row without
 * breaking the term↔detail association. `.Term` is a `<dt>`, `.Detail` is
 * a `<dd>`.
 *
 * Orientation:
 *   - `horizontal` (default): each Item is a two-column grid — the term
 *     sits in a fixed-ish first column, the detail fills the rest. This
 *     is the "label: value" details-panel look.
 *   - `vertical`: term stacks above detail (the term reads as a small
 *     caption over a larger value).
 *
 * `divider` draws a `--zs-separator` hairline between items (a logical
 * block-start border on every item after the first) so a long list reads
 * as discrete rows.
 *
 * No `role`, no live region, no asChild on the root — a description list
 * is static structured content; the semantics come entirely from the
 * native dl/dt/dd elements.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
} from "react";
import { classnames } from "../../components/_classnames";

export type DescriptionListOrientation = "horizontal" | "vertical";

export interface DescriptionListProps extends ComponentPropsWithoutRef<"dl"> {
  /**
   * Term/detail arrangement per item.
   * - `horizontal` (default): term in a fixed-ish first column, detail
   *   fills the rest (the "label: value" details-panel look).
   * - `vertical`: term stacks above detail.
   */
  orientation?: DescriptionListOrientation;

  /** Draw a `--zs-separator` hairline between items. Default `false`. */
  divider?: boolean;
}

/* ─── DescriptionList root ───────────────────────────────────────────── */

const DescriptionListRoot = forwardRef<HTMLDListElement, DescriptionListProps>(
  function DescriptionListRoot(
    { orientation = "horizontal", divider = false, className, ...rest },
    ref,
  ) {
    return (
      <dl
        {...rest}
        ref={ref}
        data-slot="description-list"
        data-orientation={orientation}
        data-divider={divider ? "" : undefined}
        className={classnames(
          "zs-description-list",
          `zs-description-list--${orientation}`,
          divider ? "zs-description-list--divided" : null,
          className,
        )}
      />
    );
  },
);
DescriptionListRoot.displayName = "DescriptionList";

/* ─── Subparts ───────────────────────────────────────────────────────── */

type DivProps = ComponentPropsWithoutRef<"div">;

export type DescriptionListItemProps = DivProps;
export type DescriptionListTermProps = ComponentPropsWithoutRef<"dt">;
export type DescriptionListDetailProps = ComponentPropsWithoutRef<"dd">;

const DescriptionListItem = forwardRef<HTMLDivElement, DescriptionListItemProps>(
  function DescriptionListItem({ className, ...rest }, ref) {
    return (
      <div
        {...rest}
        ref={ref}
        data-slot="description-list-item"
        className={classnames("zs-description-list__item", className)}
      />
    );
  },
);
DescriptionListItem.displayName = "DescriptionList.Item";

const DescriptionListTerm = forwardRef<
  HTMLElement,
  DescriptionListTermProps
>(function DescriptionListTerm({ className, ...rest }, ref) {
  return (
    <dt
      {...rest}
      ref={ref}
      data-slot="description-list-term"
      className={classnames("zs-description-list__term", className)}
    />
  );
});
DescriptionListTerm.displayName = "DescriptionList.Term";

const DescriptionListDetail = forwardRef<
  HTMLElement,
  DescriptionListDetailProps
>(function DescriptionListDetail({ className, ...rest }, ref) {
  return (
    <dd
      {...rest}
      ref={ref}
      data-slot="description-list-detail"
      className={classnames("zs-description-list__detail", className)}
    />
  );
});
DescriptionListDetail.displayName = "DescriptionList.Detail";

/* ─── public DescriptionList namespace ───────────────────────────────── */

type DescriptionListComponent = typeof DescriptionListRoot & {
  Item: typeof DescriptionListItem;
  Term: typeof DescriptionListTerm;
  Detail: typeof DescriptionListDetail;
};

export const DescriptionList =
  DescriptionListRoot as DescriptionListComponent;
DescriptionList.Item = DescriptionListItem;
DescriptionList.Term = DescriptionListTerm;
DescriptionList.Detail = DescriptionListDetail;
