/*
 * Popover — anchored panel that opens on click.
 *
 *   <Popover>
 *     <Popover.Trigger>Open</Popover.Trigger>
 *     <Popover.Portal>
 *       <Popover.Backdrop />            -- optional, default skipped
 *       <Popover.Popup>
 *         <Popover.Arrow />              -- optional, points back at trigger
 *         <Popover.Title>…</Popover.Title>
 *         <Popover.Description>…</Popover.Description>
 *         <Popover.Close>Close</Popover.Close>
 *       </Popover.Popup>
 *     </Popover.Portal>
 *   </Popover>
 *
 * Shape decisions:
 *   - Decomposed Portal / Backdrop / Popup mirrors Dialog (commit 3a64a726)
 *     so every primitive in the family reads the same way. The Popover
 *     differs in one structural detail: it positions against the trigger
 *     via Floating UI, so we mount an internal `<BasePopover.Positioner>`
 *     INSIDE Portal but OUTSIDE Popup (consumers don't reach for it).
 *     `side`, `align`, `sideOffset` live on the Popup as forwarded knobs
 *     the same way Select exposes them on Root.
 *   - Backdrop is OPT-IN. The popover-feel is anchored-panel, not
 *     scrim-darkened sheet — most popovers should not dim the page. We
 *     ship `<Popover.Backdrop>` for the modal-feel case but never auto-
 *     mount it (brief contingency).
 *   - Close mirrors Dialog.Close's `asChild` + Slot pattern (Phase 2.B
 *     review-fix item 3) so callers can wrap their own button styling
 *     without losing the close-press handler.
 *   - Arrow renders an SVG triangle scoped to the popover's themed
 *     surface color so it reads as an extension of the popup, not a
 *     separate element. Base UI 1.5 stamps `data-side` on the wrapper
 *     but does NOT auto-rotate it, so Popover.css owns side-specific
 *     SVG rotation (see `.zs-popover-arrow[data-side=...] > svg`).
 *
 * Anti-patterns we explicitly avoid (mirrored from Dialog / AlertDialog):
 *   - Auto-close glyph absolutely positioned outside the popup chrome:
 *     Popover.Close stays a real button in source order; the consumer
 *     decides where it sits.
 *   - `role="dialog"`-style focus trap on by default: Popover defers to
 *     Base UI's `modal={false}` default, which keeps the popup non-
 *     trapping. Set `modal={true}` for the rare modal popover.
 *   - Crystal material pattern: the Popup keeps a computable background-
 *     color, then layers the active rim, shadow, and material tokens.
 */
import {
  forwardRef,
  isValidElement,
  useEffect,
  useRef,
  type ComponentPropsWithoutRef,
  type MouseEvent as ReactMouseEvent,
  type Ref,
} from "react";
import { Popover as BasePopover } from "@base-ui/react/popover";
import { Button, type ButtonProps } from "../Button";
import { Slot, composeRefs } from "../_slot";
import { composeBaseClass } from "../_classnames";

export type PopoverSide = "top" | "right" | "bottom" | "left";
export type PopoverAlign = "start" | "center" | "end";

/**
 * Re-export Base UI's `createHandle` so consumers can imperatively pair
 * a Trigger to a Root (matches the Dialog pattern — Phase 2.B review-
 * fix item 11).
 */
export const createPopoverHandle = BasePopover.createHandle;

/**
 * Props for the Popover root. Generic over `Payload` so Base UI's
 * payload-render channel survives the wrapper: the Root's `children`
 * is `ReactNode | PayloadChildRenderFunction<Payload>`, exactly as the
 * Base UI source declares (`PopoverRoot.Props<Payload>` in
 * @base-ui/react/popover). The previous wrapper narrowed `children` to
 * a plain `ReactNode`, which silently rejected the documented
 * payload-render API and contradicted the lines above that promise it
 * is forwarded.
 *
 * We omit `render` for the same reason Dialog does — Root is a context
 * provider with no DOM, so `render` has no meaning at this layer.
 */
export interface PopoverProps<Payload = unknown>
  extends Omit<BasePopover.Root.Props<Payload>, "render"> {}

/* ─── Root ──────────────────────────────────────────────────────────── *
 *
 * `modal` defaults to `false` explicitly — Base UI 1.5 ships the same
 * default, but pinning it here owns the contract so a future Base UI
 * change can't silently flip popovers into focus-trapping modals. The
 * popover-feel is anchored panel; set `modal={true}` only for the rare
 * Slack-style settings popover (which should usually also opt into
 * `<Popover.Backdrop>`). */

function PopoverRoot<Payload = unknown>({
  modal = false,
  children,
  ...rest
}: PopoverProps<Payload>) {
  // Pass `children` through `<BasePopover.Root>` verbatim so a payload
  // render-function child is recognised by Base UI (instead of being
  // rendered as a literal React child).
  return (
    <BasePopover.Root<Payload> modal={modal} {...rest}>
      {children}
    </BasePopover.Root>
  );
}
PopoverRoot.displayName = "Popover";

/* ─── Trigger ───────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BasePopover.Trigger>;
export type PopoverTriggerProps = BaseTriggerProps;

/* HTMLElement (not HTMLButtonElement): Base UI's `render` lets the
 * caller swap the rendered element (e.g. `<a>`), so the consumer's
 * ref type must allow any DOM element. Same widen-then-cast trick
 * Dialog.Trigger uses (Phase 2.B review-fix item 13). */
const PopoverTrigger = forwardRef<HTMLElement, PopoverTriggerProps>(
  function PopoverTrigger(props, ref) {
    return (
      <BasePopover.Trigger
        ref={ref as Ref<HTMLButtonElement>}
        {...props}
      />
    );
  },
);
PopoverTrigger.displayName = "Popover.Trigger";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BasePopover.Portal>;
export type PopoverPortalProps = BasePortalProps;

function PopoverPortal(props: PopoverPortalProps) {
  return <BasePopover.Portal {...props} />;
}
PopoverPortal.displayName = "Popover.Portal";

/* ─── Backdrop (opt-in) ──────────────────────────────────────────────
 *
 * Default is NO backdrop — popovers are anchored panels, not modals.
 * The brief explicitly carves out a Backdrop subpart for the modal-feel
 * Popover (Slack-style settings popover, for instance), so we provide
 * the subpart but never auto-mount it. */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BasePopover.Backdrop>;
export type PopoverBackdropProps = BaseBackdropProps;

const PopoverBackdrop = forwardRef<HTMLDivElement, PopoverBackdropProps>(
  function PopoverBackdrop({ className, ...rest }, ref) {
    return (
      <BasePopover.Backdrop
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-popover-backdrop", className)}
        {...rest}
      />
    );
  },
);
PopoverBackdrop.displayName = "Popover.Backdrop";

/* ─── Popup ─────────────────────────────────────────────────────────── *
 *
 * The Popup mounts inside an internal `<BasePopover.Positioner>` so the
 * Floating UI anchoring (`side`, `align`, `sideOffset`) is configurable
 * via Popup-level props without the consumer reaching for the positioner.
 * Mirrors Select's all-in-one shape: anchor knobs on the Popup, internal
 * Positioner under the hood. */

type BasePopupProps = ComponentPropsWithoutRef<typeof BasePopover.Popup>;
export interface PopoverPopupProps extends BasePopupProps {
  /** Which side of the trigger to anchor on. Default `bottom`. */
  side?: PopoverSide;
  /** Alignment along the chosen side. Default `center`. */
  align?: PopoverAlign;
  /** Pixel offset between trigger and popup. Default `8`. */
  sideOffset?: number;
}

const PopoverPopup = forwardRef<HTMLElement, PopoverPopupProps>(
  function PopoverPopup(
    {
      side = "bottom",
      align = "center",
      sideOffset = 8,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    // Dev-only assertion mirroring Dialog.Popup (review-fix item 7):
    // Base UI renders the popup with `role="dialog"`. A `role="dialog"`
    // without an accessible name is a serious a11y bug — screen readers
    // announce "dialog" with no context. `<Popover.Title>` auto-wires
    // `aria-labelledby`; absence of BOTH a Title descendant AND an
    // explicit `aria-label` / `aria-labelledby` on the popup means no
    // accessible name. We probe the DOM after mount so Title's id has
    // landed.
    const popupRef = useRef<HTMLElement | null>(null);
    const composedRef = composeRefs<HTMLElement>(
      ref,
      popupRef as Ref<HTMLElement>,
    );
    useEffect(() => {
      if (process.env.NODE_ENV === "production") return;
      const node = popupRef.current;
      if (!node) return;
      const ariaLabel = node.getAttribute("aria-label");
      const ariaLabelledBy = node.getAttribute("aria-labelledby");
      if (!ariaLabel && !ariaLabelledBy) {
        // eslint-disable-next-line no-console
        console.warn(
          "Popover.Popup has no accessible name. Add a <Popover.Title>" +
            " (Base UI auto-wires aria-labelledby) or pass aria-label" +
            "/aria-labelledby directly.",
        );
      }
    }, []);

    return (
      <BasePopover.Positioner
        className="zs-popover-positioner"
        side={side}
        align={align}
        sideOffset={sideOffset}
      >
        <BasePopover.Popup
          {...rest}
          ref={composedRef as Ref<HTMLDivElement>}
          className={composeBaseClass("zs-popover-popup", className)}
        >
          {children}
        </BasePopover.Popup>
      </BasePopover.Positioner>
    );
  },
);
PopoverPopup.displayName = "Popover.Popup";

/* ─── Title / Description ───────────────────────────────────────────── */

type BaseTitleProps = ComponentPropsWithoutRef<typeof BasePopover.Title>;
export type PopoverTitleProps = BaseTitleProps;

const PopoverTitle = forwardRef<HTMLHeadingElement, PopoverTitleProps>(
  function PopoverTitle({ className, ...rest }, ref) {
    return (
      <BasePopover.Title
        ref={ref}
        className={composeBaseClass("zs-popover__title", className)}
        {...rest}
      />
    );
  },
);
PopoverTitle.displayName = "Popover.Title";

type BaseDescriptionProps = ComponentPropsWithoutRef<
  typeof BasePopover.Description
>;
export type PopoverDescriptionProps = BaseDescriptionProps;

const PopoverDescription = forwardRef<
  HTMLParagraphElement,
  PopoverDescriptionProps
>(function PopoverDescription({ className, ...rest }, ref) {
  return (
    <BasePopover.Description
      ref={ref}
      className={composeBaseClass("zs-popover__description", className)}
      {...rest}
    />
  );
});
PopoverDescription.displayName = "Popover.Description";

/* ─── Arrow ─────────────────────────────────────────────────────────── *
 *
 * Base UI 1.5 positions the arrow wrapper and tags it with `data-side`,
 * but does NOT rotate the wrapper itself. Popover.css rotates the inner
 * SVG per side (see `.zs-popover-arrow[data-side=...] > svg`). The
 * brief specifies `M 0,0 L 8,8 L 16,0 Z` — a 16×8 downward-pointing
 * triangle that the per-side rotation reorients so the apex always
 * points at the trigger. */

type BaseArrowProps = ComponentPropsWithoutRef<typeof BasePopover.Arrow>;
export type PopoverArrowProps = BaseArrowProps;

const PopoverArrow = forwardRef<HTMLDivElement, PopoverArrowProps>(
  function PopoverArrow({ className, children, ...rest }, ref) {
    return (
      <BasePopover.Arrow
        ref={ref}
        className={composeBaseClass("zs-popover-arrow", className)}
        {...rest}
      >
        {children ?? <ArrowGlyph />}
      </BasePopover.Arrow>
    );
  },
);
PopoverArrow.displayName = "Popover.Arrow";

function ArrowGlyph() {
  // SVG viewBox units are unitless; not subject to the no-raw-px rule.
  return (
    <svg
      width="16"
      height="8"
      viewBox="0 0 16 8"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M 0,0 L 8,8 L 16,0 Z" fill="currentColor" />
    </svg>
  );
}

/* ─── Close ─────────────────────────────────────────────────────────── *
 *
 * Same shape as Dialog.Close — `asChild` + Slot composition so callers
 * can wrap their own button styling, default path renders our Button.
 * `nativeButton` detection mirrors Phase 2.B review-fix item 2 so Base
 * UI doesn't emit a dev error when asChild swaps in a `<div>`-rendered
 * trigger. */

export interface PopoverCloseProps extends Omit<ButtonProps, "type"> {
  /** Render as the single child element instead of our Button. */
  asChild?: boolean;
}

const PopoverClose = forwardRef<HTMLButtonElement, PopoverCloseProps>(
  function PopoverClose(
    {
      asChild = false,
      variant = "gray",
      intent = "normal",
      onClick: callerOnClick,
      children,
      ...rest
    },
    ref,
  ) {
    const asChildIsNativeButton =
      asChild &&
      isValidElement(children) &&
      (children as { type?: unknown }).type === "button";
    const nativeButton = asChild ? asChildIsNativeButton : true;

    if (
      process.env.NODE_ENV !== "production" &&
      asChild &&
      !isValidElement(children)
    ) {
      // eslint-disable-next-line no-console
      console.error(
        "Popover.Close asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    return (
      <BasePopover.Close
        nativeButton={nativeButton}
        render={(closeProps) => {
          const closePropsRef = (closeProps as { ref?: Ref<unknown> }).ref;
          const closePropsOnClick = (
            closeProps as {
              onClick?: (event: ReactMouseEvent<HTMLElement>) => void;
            }
          ).onClick;

          // Caller's onClick runs first; if not preventDefault'd, Base
          // UI's close-press handler fires. Mirrors Dialog.Close's
          // composedOnClick (Phase 2.B review-fix item 1).
          const composedOnClick = (event: ReactMouseEvent<HTMLElement>) => {
            callerOnClick?.(event as ReactMouseEvent<HTMLButtonElement>);
            if (!event.defaultPrevented) {
              closePropsOnClick?.(event);
            }
          };

          if (asChild) {
            if (!isValidElement(children)) return <></>;
            // Forward `rest` (className, data-*, aria-*, disabled,
            // style) through Slot so wrapper-level attributes survive
            // the asChild render. Slot's mergeProps composes onClick
            // with the child's onClick (theirs first → ours), so we
            // pass `composedOnClick` directly — no manual extraction.
            // Mirrors Dialog.Close's shape (commit 3a64a726) extended
            // with the rest-spread fix.
            return (
              <Slot
                {...closeProps}
                {...rest}
                ref={composeRefs(ref as Ref<unknown>, closePropsRef)}
                onClick={composedOnClick}
              >
                {children}
              </Slot>
            );
          }

          // Compose our right-aligned Close marker class with the
          // caller's optional className. `rest.className` is `string |
          // undefined` (Button's prop type) so we can flatten via
          // classnames() — no need to reach for composeBaseClass's
          // callback overload here.
          const callerClassName = (rest as { className?: string }).className;
          const composedClassName = callerClassName
            ? `zs-popover__close ${callerClassName}`
            : "zs-popover__close";

          return (
            <Button
              {...closeProps}
              {...rest}
              ref={composeRefs(
                ref as Ref<HTMLElement>,
                closePropsRef as Ref<HTMLElement>,
              )}
              onClick={composedOnClick}
              variant={variant}
              intent={intent}
              className={composedClassName}
            >
              {children}
            </Button>
          );
        }}
      />
    );
  },
);
PopoverClose.displayName = "Popover.Close";

/* ─── public namespace ──────────────────────────────────────────────── */

export type PopoverComponent = typeof PopoverRoot & {
  Trigger: typeof PopoverTrigger;
  Portal: typeof PopoverPortal;
  Backdrop: typeof PopoverBackdrop;
  Popup: typeof PopoverPopup;
  Title: typeof PopoverTitle;
  Description: typeof PopoverDescription;
  Close: typeof PopoverClose;
  Arrow: typeof PopoverArrow;
  createHandle: typeof createPopoverHandle;
};

export const Popover = PopoverRoot as PopoverComponent;
Popover.Trigger = PopoverTrigger;
Popover.Portal = PopoverPortal;
Popover.Backdrop = PopoverBackdrop;
Popover.Popup = PopoverPopup;
Popover.Title = PopoverTitle;
Popover.Description = PopoverDescription;
Popover.Close = PopoverClose;
Popover.Arrow = PopoverArrow;
Popover.createHandle = createPopoverHandle;
