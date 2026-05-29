/*
 * Drawer — side-anchored modal panel for navigation, secondary forms,
 * filter trays, settings shelves. A Drawer is conceptually a Dialog
 * pinned to one edge of the viewport instead of centered; we reuse
 * Base UI's `Dialog` primitive verbatim and only override positioning
 * + the side-aware enter/leave transform.
 *
 *   <Drawer>
 *     <Drawer.Trigger>Open</Drawer.Trigger>
 *     <Drawer.Portal>
 *       <Drawer.Backdrop />
 *       <Drawer.Content side="end" size="md">
 *         <Drawer.Header>
 *           <Drawer.Title>Filter results</Drawer.Title>
 *           <Drawer.Description>…</Drawer.Description>
 *         </Drawer.Header>
 *         <Drawer.Body>…</Drawer.Body>
 *         <Drawer.Footer>
 *           <Drawer.Close>Cancel</Drawer.Close>
 *           <Button>Apply</Button>
 *         </Drawer.Footer>
 *       </Drawer.Content>
 *     </Drawer.Portal>
 *   </Drawer>
 *
 * Logical sides only — `start` / `end` flip naturally under RTL via
 * CSS logical properties (`inset-inline-start`, `inset-inline-end`).
 * We do NOT expose `left` / `right`; consumers describe intent, the
 * stylesheet picks the physical edge from `dir`.
 *
 * Slot order:
 *   - Consumer aria/data passthrough props spread LAST so the consumer
 *     wins.
 *   - EXCEPTION (mirrors Dialog item 14): variant-driven internal
 *     `data-side` / `data-size` attributes that select CSS rules must
 *     win — we spread `{...rest}` FIRST and append our `data-*` AFTER.
 *
 * Anti-patterns we explicitly avoid:
 *  - No `left`/`right` props — logical sides only (RTL must flip
 *    without a re-render).
 *  - No nested Dialog/Drawer inside Drawer.Content (Base UI's portal
 *    handles stacking but the brief calls this out as a smell).
 *  - Don't auto-focus the Close button — let Base UI's focus trap
 *    pick the first focusable so the typical "skim → tab → act" flow
 *    starts on actual content, not the dismiss control.
 *
 * Crystal styling lives in `Drawer.css`. The Content carries an
 * opaque `background-color: var(--zs-surface)` so axe's color-contrast
 * walk terminates on a solid surface even if the theme adds a
 * `backdrop-filter` for the over-content glass effect (glass-surface
 * invariant: opaque base + backdrop-filter NEVER alone).
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type Ref,
} from "react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Button, type ButtonProps } from "../Button";
import { Slot, composeRefs } from "../_slot";
import { classnames, composeBaseClass } from "../_classnames";

/**
 * Side the drawer anchors to. Logical values flip under RTL:
 * - `start` reads left in LTR, right in RTL.
 * - `end` reads right in LTR, left in RTL.
 * - `top` / `bottom` are direction-invariant.
 */
export type DrawerSide = "start" | "end" | "top" | "bottom";

/**
 * Inline-size preset for horizontal drawers (`side="start" | "end"`) or
 * block-size preset for vertical drawers (`side="top" | "bottom"`).
 * - `sm` = 16rem.
 * - `md` = 24rem (default).
 * - `lg` = 32rem.
 * - `full` = 100% of the cross axis (e.g. full-screen takeover).
 */
export type DrawerSize = "sm" | "md" | "lg" | "full";

type BaseRootProps = ComponentPropsWithRef<typeof BaseDialog.Root>;

/**
 * Props for the Drawer root. Derived from Base UI's `Dialog.Root` so
 * every Base UI escape hatch — `onOpenChangeComplete`, `actionsRef`,
 * `handle`, `triggerId`, the payload child-render function — is
 * forwarded verbatim.
 */
export interface DrawerProps extends BaseRootProps {
  /**
   * Whether the drawer is modal (focus trap + scroll lock + backdrop
   * blocks outside interaction). Default `true`. Set `false` for
   * navigation drawers that should leave the document interactive
   * (rare; typically the modal-with-backdrop pattern is right).
   */
  modal?: boolean | "trap-focus";
  /** Drawer subtree — Trigger + Portal + Backdrop + Content. */
  children?: ReactNode;
}

/* ─── Root ──────────────────────────────────────────────────────────── */

function DrawerRoot({ modal = true, children, ...rest }: DrawerProps) {
  return (
    <BaseDialog.Root {...rest} modal={modal}>
      {children}
    </BaseDialog.Root>
  );
}
DrawerRoot.displayName = "Drawer";

/* ─── Trigger ───────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseDialog.Trigger>;
/**
 * Props for `Drawer.Trigger`. Base UI's Trigger drives `aria-haspopup`
 * + `aria-expanded` on the underlying element; we pass through `render`
 * so consumers can swap in a `<Button>` (the conventional pattern).
 */
export type DrawerTriggerProps = BaseTriggerProps;

/* HTMLElement (not HTMLButtonElement): Base UI's `render` lets the
 * caller swap the rendered element (e.g. `<a>`), so the consumer's
 * ref type must allow any DOM element. */
const DrawerTrigger = forwardRef<HTMLElement, DrawerTriggerProps>(
  function DrawerTrigger(props, ref) {
    return (
      <BaseDialog.Trigger
        ref={ref as Ref<HTMLButtonElement>}
        {...props}
      />
    );
  },
);
DrawerTrigger.displayName = "Drawer.Trigger";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseDialog.Portal>;
/** Props for `Drawer.Portal`. Base UI's Portal renders the subtree at
 * the document body; pass `container` to redirect. */
export type DrawerPortalProps = BasePortalProps;

function DrawerPortal(props: DrawerPortalProps) {
  return <BaseDialog.Portal {...props} />;
}
DrawerPortal.displayName = "Drawer.Portal";

/* ─── Backdrop ──────────────────────────────────────────────────────── */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BaseDialog.Backdrop>;
/** Props for `Drawer.Backdrop`. Renders a fixed-position overlay that
 * dims the document and (when `modal=true`) blocks outside interaction.
 * Click-through closes the drawer unless `dismissible=false` is wired
 * by the consumer (Base UI's `onOpenChange` reason mechanism). */
export type DrawerBackdropProps = BaseBackdropProps;

const DrawerBackdrop = forwardRef<HTMLElement, DrawerBackdropProps>(
  function DrawerBackdrop({ className, ...rest }, ref) {
    return (
      <BaseDialog.Backdrop
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-drawer-backdrop", className)}
      />
    );
  },
);
DrawerBackdrop.displayName = "Drawer.Backdrop";

/* ─── Content ───────────────────────────────────────────────────────── */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseDialog.Popup>;
/** Props for `Drawer.Content`. The anchor edge is logical (`side`); the
 * cross-axis size is preset (`size`). */
export interface DrawerContentProps extends BasePopupProps {
  /**
   * Logical side the drawer anchors to. Default `end` (right in LTR).
   * `start` / `end` flip under RTL automatically via CSS logical
   * properties — do not pass `left` / `right`.
   */
  side?: DrawerSide;
  /**
   * Cross-axis size preset. For `side="start" | "end"` this maps to
   * inline-size; for `side="top" | "bottom"` it maps to block-size.
   * Default `md`.
   */
  size?: DrawerSize;
}

const DrawerContent = forwardRef<HTMLElement, DrawerContentProps>(
  function DrawerContent(
    { side = "end", size = "md", className, ...rest },
    ref,
  ) {
    // Internal `data-side` / `data-size` select CSS variants — spread
    // {...rest} FIRST so the consumer can't clobber them (variant
    // exception, mirrors Dialog item 14).
    return (
      <BaseDialog.Popup
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-drawer-content", className)}
        data-side={side}
        data-size={size}
      />
    );
  },
);
DrawerContent.displayName = "Drawer.Content";

/* ─── Header ────────────────────────────────────────────────────────── */

/** Props for `Drawer.Header` — layout-only flex row carrying Title,
 * Description, and (optionally) the auto-X close. */
export interface DrawerHeaderProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Show the auto-X close button inside the header. Default `true`.
   * Set `false` for forced-action drawers where the only exits are
   * the Footer buttons.
   */
  showClose?: boolean;
  /**
   * Aria-label for the auto-X close button. Defaults to `"Close"`.
   * Localized consumers override.
   */
  closeLabel?: string;
}

const DrawerHeader = forwardRef<HTMLDivElement, DrawerHeaderProps>(
  function DrawerHeader(
    { showClose = true, closeLabel = "Close", className, children, ...rest },
    ref,
  ) {
    return (
      <div
        ref={ref}
        className={classnames("zs-drawer__header", className)}
        {...rest}
      >
        <div className="zs-drawer__header-content">{children}</div>
        {showClose ? (
          <BaseDialog.Close
            className="zs-drawer__header-close"
            aria-label={closeLabel}
          >
            <CloseGlyph />
          </BaseDialog.Close>
        ) : null}
      </div>
    );
  },
);
DrawerHeader.displayName = "Drawer.Header";

function CloseGlyph() {
  // SVG viewBox units are unitless; not subject to the no-raw-px rule.
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M4 4l8 8M12 4l-8 8"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.75"
        strokeLinecap="round"
      />
    </svg>
  );
}

/* ─── Title / Description ───────────────────────────────────────────── */

type BaseTitleProps = ComponentPropsWithoutRef<typeof BaseDialog.Title>;
/** Props for `Drawer.Title`. Base UI auto-wires
 * `aria-labelledby` on Content. */
export type DrawerTitleProps = BaseTitleProps;
const DrawerTitle = forwardRef<HTMLHeadingElement, DrawerTitleProps>(
  function DrawerTitle({ className, ...rest }, ref) {
    return (
      <BaseDialog.Title
        ref={ref}
        className={composeBaseClass("zs-drawer__title", className)}
        {...rest}
      />
    );
  },
);
DrawerTitle.displayName = "Drawer.Title";

type BaseDescriptionProps = ComponentPropsWithoutRef<typeof BaseDialog.Description>;
/** Props for `Drawer.Description`. Base UI auto-wires
 * `aria-describedby` on Content. */
export type DrawerDescriptionProps = BaseDescriptionProps;
const DrawerDescription = forwardRef<HTMLParagraphElement, DrawerDescriptionProps>(
  function DrawerDescription({ className, ...rest }, ref) {
    return (
      <BaseDialog.Description
        ref={ref}
        className={composeBaseClass("zs-drawer__description", className)}
        {...rest}
      />
    );
  },
);
DrawerDescription.displayName = "Drawer.Description";

/* ─── Body / Footer (layout-only) ───────────────────────────────────── */

/** Props for `Drawer.Body` — scroll region between Header and Footer. */
export type DrawerBodyProps = ComponentPropsWithoutRef<"div">;
const DrawerBody = forwardRef<HTMLDivElement, DrawerBodyProps>(
  function DrawerBody({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-drawer__body", className)}
        {...rest}
      />
    );
  },
);
DrawerBody.displayName = "Drawer.Body";

/** Props for `Drawer.Footer` — trailing action row. */
export type DrawerFooterProps = ComponentPropsWithoutRef<"div">;
const DrawerFooter = forwardRef<HTMLDivElement, DrawerFooterProps>(
  function DrawerFooter({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-drawer__footer", className)}
        {...rest}
      />
    );
  },
);
DrawerFooter.displayName = "Drawer.Footer";

/* ─── Close ─────────────────────────────────────────────────────────── */

/** Props for `Drawer.Close`. Extends our Button surface; pair with
 * `asChild` to swap in a custom element while keeping Base UI's
 * close-on-press behavior. */
export interface DrawerCloseProps extends Omit<ButtonProps, "type"> {
  /**
   * Render as the single child element rather than our Button. Used
   * when the caller wants custom button styling but the Base UI
   * close-on-press behavior. Routed through the shared `_slot.ts`
   * helper so className, style, refs, AND event handlers compose
   * (mirrors `Dialog.Close 3a64a726`). Pass a single React element
   * child — anything else is a dev-error.
   */
  asChild?: boolean;
}

/**
 * Drawer.Close — styled wrapper around our Button that participates in
 * Base UI's close-on-press machinery. Renders a `<BaseDialog.Close>`
 * with our Button inside via render-prop. Default variant is `gray`
 * (neutral cancel). Override with `variant="filled"` for primary
 * confirm buttons.
 *
 * The default ref typing is `HTMLButtonElement` — consumers' Button
 * refs typecheck against the default path. The `asChild` path carries
 * the constraint that the caller's child is responsible for accepting
 * that ref shape.
 */
const DrawerClose = forwardRef<HTMLButtonElement, DrawerCloseProps>(
  function DrawerClose(
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
    // Detect whether the asChild target is a native `<button>` so we
    // can drive `nativeButton` correctly. Base UI emits a dev error AND
    // applies non-native handlers (role="button", keyboard handlers)
    // when this is mismatched.
    const asChildIsNativeButton =
      asChild &&
      isValidElement(children) &&
      (children as { type?: unknown }).type === "button";

    // For the default path we render our Button which renders a real
    // <button>, so `nativeButton={true}`. For asChild paths we trust
    // the inspection above.
    const nativeButton = asChild ? asChildIsNativeButton : true;

    if (process.env.NODE_ENV !== "production" && asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.error(
        "Drawer.Close asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    return (
      <BaseDialog.Close
        nativeButton={nativeButton}
        render={(closeProps) => {
          const closePropsRef = (closeProps as { ref?: Ref<unknown> }).ref;
          const closePropsOnClick = (
            closeProps as {
              onClick?: (event: ReactMouseEvent<HTMLElement>) => void;
            }
          ).onClick;

          // Compose caller onClick with Base UI's close handler.
          // Caller runs first; if they preventDefault, close is
          // skipped — mirrors Dialog.Close's pattern (commit 3a64a726).
          const composedOnClick = (event: ReactMouseEvent<HTMLElement>) => {
            callerOnClick?.(event as ReactMouseEvent<HTMLButtonElement>);
            if (!event.defaultPrevented) {
              closePropsOnClick?.(event);
            }
          };

          if (asChild) {
            if (!isValidElement(children)) {
              // Render an empty fragment so Base UI's render-prop
              // contract (returns a ReactElement) is satisfied; the
              // dev-error above already flagged the misuse.
              return <></>;
            }
            // Slot handles className / style / event composition and
            // ref fan-out — including the child's own onClick AND ref
            // via `mergeProps` + `composeRefs(ourRef, childRef)`
            // (see `_slot.ts`). We MUST therefore pass only the
            // wrapper-level composed handler (caller → close) to Slot
            // and let Slot stitch the child's onClick in front of it.
            // The previous implementation invoked `childOnClick`
            // manually inside `slotOnClick`, so the child handler ran
            // twice per click — a violation of the _slot.ts contract.
            // It also re-included `getElementRef(children)` in the
            // composeRefs chain even though Slot already composes the
            // child ref internally, causing a redundant setRef call
            // on the same node.
            //
            // Slot prop order:
            //   1. closeProps   — Base UI's nativeButton/role bits.
            //   2. ...rest      — consumer props on <Drawer.Close>.
            //   3. ref + onClick — composed last so they win.
            //
            // onClick composition order (Slot-driven): child onClick
            // → caller onClick → Base UI close. Any preventDefault
            // short-circuits the remaining handlers (mirrors
            // Dialog.Close 3a64a726).
            return (
              <Slot
                {...closeProps}
                {...rest}
                ref={composeRefs(
                  ref as Ref<unknown>,
                  closePropsRef,
                )}
                onClick={composedOnClick}
              >
                {children}
              </Slot>
            );
          }

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
            >
              {children}
            </Button>
          );
        }}
      />
    );
  },
);
DrawerClose.displayName = "Drawer.Close";

/* ─── public namespace ──────────────────────────────────────────────── */

type DrawerComponent = typeof DrawerRoot & {
  Trigger: typeof DrawerTrigger;
  Portal: typeof DrawerPortal;
  Backdrop: typeof DrawerBackdrop;
  Content: typeof DrawerContent;
  Header: typeof DrawerHeader;
  Title: typeof DrawerTitle;
  Description: typeof DrawerDescription;
  Body: typeof DrawerBody;
  Footer: typeof DrawerFooter;
  Close: typeof DrawerClose;
};

export const Drawer = DrawerRoot as DrawerComponent;
Drawer.Trigger = DrawerTrigger;
Drawer.Portal = DrawerPortal;
Drawer.Backdrop = DrawerBackdrop;
Drawer.Content = DrawerContent;
Drawer.Header = DrawerHeader;
Drawer.Title = DrawerTitle;
Drawer.Description = DrawerDescription;
Drawer.Body = DrawerBody;
Drawer.Footer = DrawerFooter;
Drawer.Close = DrawerClose;
