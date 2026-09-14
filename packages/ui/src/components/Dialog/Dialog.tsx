/*
 * Dialog — modal popup. Subparts mirror Base UI's headless
 * shape directly:
 *
 *   <Dialog>
 *     <Dialog.Trigger>Open</Dialog.Trigger>
 *     <Dialog.Portal>
 *       <Dialog.Backdrop />
 *       <Dialog.Popup>
 *         <Dialog.Header>
 *           <Dialog.Title>…</Dialog.Title>
 *           <Dialog.Description>…</Dialog.Description>
 *         </Dialog.Header>
 *         <Dialog.Body>…</Dialog.Body>
 *         <Dialog.Footer>
 *           <Dialog.Close>Cancel</Dialog.Close>
 *           <Button>Save</Button>
 *         </Dialog.Footer>
 *       </Dialog.Popup>
 *     </Dialog.Portal>
 *   </Dialog>
 *
 * The decomposed shape is the canonical write. Header/Body/Footer are
 * layout-only divs — they carry no ARIA semantics. Title and
 * Description are Base UI passthroughs that auto-feed `aria-labelledby`
 * and `aria-describedby` on the Popup.
 *
 * Slot order:
 *   - For consumer aria/data passthrough props, we spread `{...rest}`
 *     LAST so the consumer wins.
 *   - EXCEPTION (review-fix item 14): for variant-driven internal
 *     `data-size`/`data-tint`/`data-placement` attributes that select
 *     CSS rules, the internal value MUST win. We spread `{...rest}`
 *     FIRST in those subparts and append our `data-*` AFTER. This
 *     keeps the variant→CSS pipeline intact even if the consumer hands
 *     us `data-size` directly.
 *
 * Base UI handles every aria-wiring detail — we never overwrite its
 * attributes.
 *
 * Surface narrowing:
 *   - We deliberately re-export `createHandle` from Base UI (review-fix
 *     item 11). `Dialog.Viewport` is exposed as a styled passthrough so
 *     consumers can position dialogs inside scroll containers without
 *     reaching into Base UI directly.
 *
 * Anti-patterns we explicitly avoid:
 *  - 3: auto-X close button absolutely positioned outside the header.
 *    Our Header is a flex row; the close lives INSIDE the Header so
 *    the layout never pushes it outside the visible chrome.
 *  - 4: per-button props on Dialog (`primaryAction={{...}}`). We use
 *    children composition only.
 *  - 5: NO role distinction between alert and regular. Dialog renders
 *    `role="dialog"` (via Base UI); AlertDialog is a SEPARATE
 *    component that renders `role="alertdialog"`.
 *  - 7: Dialog without focus restore. Base UI's `finalFocus` handles
 *    this — focus returns to the Trigger on close.
 *  - 8: static methods (`Dialog.confirm()`). No.
 *  - 10: `min-height: 100vh`. Our CSS uses `100dvh`.
 *  - 11: No aria-describedby on dialogs with descriptions. Base UI
 *    auto-wires it.
 *  - 12: disabling outside-click but not ESC. Our `dismissible={false}`
 *    disables BOTH (outside-press AND escape-key are cancelled in
 *    onOpenChange).
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useContext,
  useEffect,
  useRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type MouseEvent as ReactMouseEvent,
  type Ref,
} from "react";
import { X } from "lucide-react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Button, type ButtonProps } from "../Button";
import { Icon } from "../Icon";
import { Slot, composeRefs } from "../_slot";
import { classnames, composeBaseClass } from "../_classnames";

export type DialogSize = "sm" | "md" | "lg" | "full";
export type DialogPlacement = "center" | "top";
/**
 * Backdrop tint.
 * - `scrim` (default): a 40% black overlay — standard sheet backdrop.
 * - `material`: a translucent surface with backdrop-filter — for
 *   sheets layered over content-rich backgrounds.
 * - `invisible`: a transparent click-blocker — for non-modal dialogs
 *   that need to block interaction without a visible dim. Renamed from
 *   the older `"none"` (review-fix item 10) to make the click-blocking
 *   semantics explicit; pairing `tint="invisible"` with `modal={true}`
 *   dev-warns since it creates an invisible interaction trap.
 */
export type DialogBackdropTint = "scrim" | "material" | "invisible";

type BaseRootProps = ComponentPropsWithRef<typeof BaseDialog.Root>;
type BaseChangeEventDetails = Parameters<
  NonNullable<BaseRootProps["onOpenChange"]>
>[1];

/* Re-export Base UI's createHandle so consumers can imperatively pair
 * a Trigger to a Root (review-fix item 11). */
export const createDialogHandle = BaseDialog.createHandle;

/* Internal context — exposes the Root's `modal` value so Backdrop can
 * dev-warn on the `tint="invisible" + modal=true` hazard (review-fix
 * item 10). NOT exported. */
type DialogRootRuntimeContext = {
  modal: boolean | "trap-focus";
};
const DialogRootRuntimeCtx = createContext<DialogRootRuntimeContext | null>(
  null,
);

/**
 * Props for the Dialog root. Derived from Base UI's `Dialog.Root` so
 * every Base UI escape hatch — `onOpenChangeComplete`, `actionsRef`,
 * `handle`, `triggerId`, `defaultTriggerId`, the payload child-render
 * function — is forwarded verbatim (review-fix item 6). We omit
 * `disablePointerDismissal` because we expose it under the friendlier
 * `dismissible` name with inverted semantics (see below).
 */
export interface DialogProps
  extends Omit<BaseRootProps, "disablePointerDismissal"> {
  /**
   * Whether the dialog can be dismissed by outside-press OR Escape.
   * Default `true`. Setting `false` enforces non-dismissibility for
   * BOTH inputs (anti-pattern #12: never disable outside-click but
   * leave ESC). The user must use a Dialog.Close button to dismiss.
   */
  dismissible?: boolean;
  /**
   * Subparts of the dialog (Trigger, Portal, Backdrop, Popup, …) — or
   * a Base UI payload render function `({ payload }) => ReactNode`
   * that receives the typed payload from the active `Dialog.Trigger`
   * (its `payload` prop) and returns the subparts. Forwarded verbatim
   * to `BaseDialog.Root` so the typed payload-handle flow keeps
   * working; do NOT narrow to `ReactNode`, that drops the
   * render-function branch and makes `createDialogHandle` half-broken.
   */
  children?: BaseRootProps["children"];
}

/* `composeBaseClass` now lives in `../_classnames` (review-fix item 10,
 * Phase 2.C) — Dialog, AlertDialog, and Field used byte-identical
 * copies; one source of truth keeps the surface honest. */

/* ─── Root ──────────────────────────────────────────────────────────── */

function DialogRoot({
  onOpenChange,
  modal = true,
  dismissible = true,
  children,
  ...rest
}: DialogProps) {
  // `dismissible={false}` enforces non-dismissibility for BOTH
  // outside-press (via Base UI's `disablePointerDismissal`) AND
  // escape-key (via cancelling in the onOpenChange handler when the
  // reason is 'escape-key'). Anti-pattern #12: disabling outside-click
  // without ESC.
  //
  // Base UI 1.5 semantics (review-fix item 5): `disablePointerDismissal`
  // is the inverse of "dismissible" — `false` (the default) means the
  // dialog DOES close on outside-press. So `!dismissible` is the
  // correct mapping. If Base UI ever flips this default (or renames),
  // a Base UI upgrade reader needs to revisit this line.
  const handleOpenChange = (
    nextOpen: boolean,
    details: BaseChangeEventDetails,
  ) => {
    if (!dismissible && nextOpen === false) {
      const reason = details.reason;
      if (reason === "escape-key" || reason === "outside-press") {
        details.cancel();
        return;
      }
    }
    onOpenChange?.(nextOpen, details);
  };

  return (
    <DialogRootRuntimeCtx.Provider value={{ modal }}>
      <BaseDialog.Root
        {...rest}
        onOpenChange={handleOpenChange}
        modal={modal}
        disablePointerDismissal={!dismissible}
      >
        {children}
      </BaseDialog.Root>
    </DialogRootRuntimeCtx.Provider>
  );
}
DialogRoot.displayName = "Dialog";

/* ─── Trigger ───────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseDialog.Trigger>;
export type DialogTriggerProps = BaseTriggerProps;

/* HTMLElement (not HTMLButtonElement): Base UI's `render` lets the
 * caller swap the rendered element (e.g. `<a>`), so the consumer's
 * ref type must allow any DOM element (review-fix item 13).
 *
 * No internal class hook (review-fix item 20): the previous
 * `zs-dialog-trigger` class was never used by any CSS rule. The
 * Trigger is typically replaced via `render={<Button>}` and the
 * Button supplies all its own styling — adding a dead hook here only
 * costs bytes. Consumers who want a hook pass `className` via rest. */
const DialogTrigger = forwardRef<HTMLElement, DialogTriggerProps>(
  function DialogTrigger(props, ref) {
    // Base UI declares Trigger as HTMLButtonElement-typed; we widen to
    // HTMLElement for the consumer surface but cast at the boundary.
    return (
      <BaseDialog.Trigger
        ref={ref as Ref<HTMLButtonElement>}
        {...props}
      />
    );
  },
);
DialogTrigger.displayName = "Dialog.Trigger";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseDialog.Portal>;
export type DialogPortalProps = BasePortalProps;

function DialogPortal(props: DialogPortalProps) {
  return <BaseDialog.Portal {...props} />;
}
DialogPortal.displayName = "Dialog.Portal";

/* ─── Viewport (positioning / scroll boundary) ──────────────────────── */

type BaseViewportProps = ComponentPropsWithoutRef<typeof BaseDialog.Viewport>;
export type DialogViewportProps = BaseViewportProps;

const DialogViewport = forwardRef<HTMLDivElement, DialogViewportProps>(
  function DialogViewport({ className, ...rest }, ref) {
    return (
      <BaseDialog.Viewport
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-dialog-viewport", className)}
        {...rest}
      />
    );
  },
);
DialogViewport.displayName = "Dialog.Viewport";

/* ─── Backdrop ──────────────────────────────────────────────────────── */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BaseDialog.Backdrop>;
export interface DialogBackdropProps extends BaseBackdropProps {
  /**
   * Backdrop tint. See `DialogBackdropTint` for the meaning of each
   * value. `invisible` paired with `modal={true}` is a hazard (an
   * invisible interaction blocker); we dev-warn on that combination.
   */
  tint?: DialogBackdropTint;
}

const DialogBackdrop = forwardRef<HTMLElement, DialogBackdropProps>(
  function DialogBackdrop({ tint = "scrim", className, ...rest }, ref) {
    const root = useContext(DialogRootRuntimeCtx);
    // Dev-warn the invisible-modal-blocker hazard (review-fix item 10).
    // `tint="invisible"` + `modal=true` paints a fully transparent
    // backdrop that still traps interaction — a click-blocker no user
    // can see. Modal defaults to true, so "no modal prop set" counts.
    useEffect(() => {
      if (process.env.NODE_ENV === "production") return;
      const modal = root?.modal ?? true;
      if (tint === "invisible" && modal !== false) {
        // eslint-disable-next-line no-console
        console.warn(
          'Dialog.Backdrop tint="invisible" combined with modal' +
            ' (default) renders a fully transparent click-blocker.' +
            ' Either set modal={false} or pick a visible tint' +
            ' ("scrim" or "material").',
        );
      }
    }, [tint, root]);

    // Internal `data-tint` selects the CSS variant. Spread {...rest}
    // FIRST so we win for variant-driven attrs (item 14 exception).
    return (
      <BaseDialog.Backdrop
        {...rest}
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-dialog-backdrop", className)}
        data-tint={tint}
      />
    );
  },
);
DialogBackdrop.displayName = "Dialog.Backdrop";

/* ─── Popup ─────────────────────────────────────────────────────────── */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseDialog.Popup>;
export interface DialogPopupProps extends BasePopupProps {
  /**
   * Maximum inline-size of the popup. Default `md`. Note that
   * `size="full"` is a full-viewport takeover; if you also pass
   * `placement="top"`, the `full` mode wins and placement is ignored
   * (we dev-warn — review-fix item 22).
   */
  size?: DialogSize;
  /** Vertical placement. Default `center`. */
  placement?: DialogPlacement;
}

const DialogPopup = forwardRef<HTMLElement, DialogPopupProps>(
  function DialogPopup(
    { size = "md", placement = "center", className, ...rest },
    ref,
  ) {
    const popupRef = useRef<HTMLElement | null>(null);
    const composedRef = composeRefs<HTMLElement>(
      ref,
      popupRef as Ref<HTMLElement>,
    );

    // Dev-only assertions (review-fix items 7 + 22):
    //  - Warn when Popup renders without aria-label/aria-labelledby
    //    (Base UI auto-wires labelledby when <Dialog.Title> is a
    //    descendant; absence of both means no accessible name).
    //  - Warn when `size="full"` is combined with `placement="top"`,
    //    since `full` wins and the placement is silently dropped.
    // The hook is called unconditionally; the body short-circuits in
    // production (and tsup DCEs it out).
    useEffect(() => {
      if (process.env.NODE_ENV === "production") return;
      if (size === "full" && placement === "top") {
        // eslint-disable-next-line no-console
        console.warn(
          'Dialog.Popup size="full" ignores placement="top" — the full' +
            " takeover anchors to the inset edges. Drop one of the props.",
        );
      }
      const node = popupRef.current;
      if (!node) return;
      const ariaLabel = node.getAttribute("aria-label");
      const ariaLabelledBy = node.getAttribute("aria-labelledby");
      if (!ariaLabel && !ariaLabelledBy) {
        // eslint-disable-next-line no-console
        console.warn(
          "Dialog.Popup has no accessible name. Add a <Dialog.Title>" +
            " (Base UI auto-wires aria-labelledby) or pass aria-label" +
            "/aria-labelledby directly.",
        );
      }
    }, [size, placement]);

    // Internal data-size / data-placement select CSS variants — spread
    // {...rest} FIRST so consumer can't clobber them (item 14 exception).
    return (
      <BaseDialog.Popup
        {...rest}
        ref={composedRef as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-dialog-popup", className)}
        data-size={size}
        data-placement={placement}
      />
    );
  },
);
DialogPopup.displayName = "Dialog.Popup";

/* ─── Header ────────────────────────────────────────────────────────── */

export interface DialogHeaderProps extends ComponentPropsWithoutRef<"div"> {
  /**
   * Show the auto-X close button inside the header.
   * Default `true`. Set `false` for forced-action dialogs where the
   * only exits are the Footer buttons. Anti-pattern #3: NEVER absolute-
   * position the close OUTSIDE the header — ours is a flex child so
   * the layout always contains it.
   */
  showClose?: boolean;
  /**
   * Aria-label for the auto-X close button. Defaults to `"Close"`.
   * Localized consumers override.
   */
  closeLabel?: string;
}

const DialogHeader = forwardRef<HTMLDivElement, DialogHeaderProps>(
  function DialogHeader(
    { showClose = true, closeLabel = "Close", className, children, ...rest },
    ref,
  ) {
    return (
      <div
        ref={ref}
        className={classnames("zs-dialog__header", className)}
        {...rest}
      >
        <div className="zs-dialog__header-content">{children}</div>
        {showClose ? (
          <BaseDialog.Close
            className="zs-dialog__header-close"
            aria-label={closeLabel}
          >
            <Icon as={X} size="sm" />
          </BaseDialog.Close>
        ) : null}
      </div>
    );
  },
);
DialogHeader.displayName = "Dialog.Header";

/* ─── Title / Description ───────────────────────────────────────────── */

type BaseTitleProps = ComponentPropsWithoutRef<typeof BaseDialog.Title>;
export type DialogTitleProps = BaseTitleProps;
const DialogTitle = forwardRef<HTMLHeadingElement, DialogTitleProps>(
  function DialogTitle({ className, ...rest }, ref) {
    return (
      <BaseDialog.Title
        ref={ref}
        className={composeBaseClass("zs-dialog__title", className)}
        {...rest}
      />
    );
  },
);
DialogTitle.displayName = "Dialog.Title";

type BaseDescriptionProps = ComponentPropsWithoutRef<typeof BaseDialog.Description>;
export type DialogDescriptionProps = BaseDescriptionProps;
const DialogDescription = forwardRef<HTMLParagraphElement, DialogDescriptionProps>(
  function DialogDescription({ className, ...rest }, ref) {
    return (
      <BaseDialog.Description
        ref={ref}
        className={composeBaseClass("zs-dialog__description", className)}
        {...rest}
      />
    );
  },
);
DialogDescription.displayName = "Dialog.Description";

/* ─── Body / Footer (layout-only) ───────────────────────────────────── */

export type DialogBodyProps = ComponentPropsWithoutRef<"div">;
const DialogBody = forwardRef<HTMLDivElement, DialogBodyProps>(
  function DialogBody({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-dialog__body", className)}
        {...rest}
      />
    );
  },
);
DialogBody.displayName = "Dialog.Body";

export type DialogFooterProps = ComponentPropsWithoutRef<"div">;
const DialogFooter = forwardRef<HTMLDivElement, DialogFooterProps>(
  function DialogFooter({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames("zs-dialog__footer", className)}
        {...rest}
      />
    );
  },
);
DialogFooter.displayName = "Dialog.Footer";

/* ─── Close ─────────────────────────────────────────────────────────── */

export interface DialogCloseProps extends Omit<ButtonProps, "type"> {
  /**
   * Render as the single child element rather than our Button. Used
   * when the caller wants a custom button styling but the Base UI
   * close-on-press behavior. Routed through the shared Slot helper so
   * className, style, refs, AND event handlers compose (review-fix
   * item 3). Pass a single React element child — anything else is a
   * dev-error.
   */
  asChild?: boolean;
}

/**
 * Dialog.Close — styled wrapper around our Button that participates in
 * Base UI's close-on-press machinery. Renders a `<BaseDialog.Close>`
 * with our Button inside via render-prop. Default variant is `gray`
 * (neutral cancel). Override with `variant="filled"` for primary
 * confirm buttons.
 *
 * The default ref typing is `HTMLButtonElement` (review-fix item 15) —
 * consumers' `useRef<HTMLButtonElement>(null)` typechecks against the
 * default path. The `asChild` path carries the constraint that the
 * caller's child is responsible for accepting that ref shape.
 */
const DialogClose = forwardRef<HTMLButtonElement, DialogCloseProps>(
  function DialogClose(
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
    // can drive `nativeButton` correctly (review-fix item 2). Base UI
    // emits a dev error AND applies non-native handlers (role="button",
    // keyboard handlers) when this is mismatched.
    const asChildIsNativeButton =
      asChild && isValidElement(children) && (children as { type?: unknown }).type === "button";

    // For the default path we render our Button which renders a real
    // <button>, so `nativeButton={true}`. For asChild paths we trust
    // the inspection above.
    const nativeButton = asChild ? asChildIsNativeButton : true;

    if (process.env.NODE_ENV !== "production" && asChild && !isValidElement(children)) {
      // eslint-disable-next-line no-console
      console.error(
        "Dialog.Close asChild expects a single React element child; received " +
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
          // skipped — mirrors AlertDialog.Action's pattern (review-fix
          // item 1).
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
            // ref fan-out, including React 19's `element.props.ref`
            // shape (review-fix item 3). Caller's onClick on the child
            // composes with the close handler we pass in.
            //
            // Spread `{...rest}` AFTER `{...closeProps}` so wrapper
            // props on `<Dialog.Close asChild>` — `className`,
            // `data-*`, `aria-*`, `style`, `disabled` — reach the
            // rendered child via Slot's mergeProps. Mirrors
            // AlertDialog.Cancel's asChild branch. Wave6 fix: the
            // prior implementation dropped `...rest` entirely, so
            // `<Dialog.Close asChild data-foo="bar">` would silently
            // discard `data-foo`.
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
DialogClose.displayName = "Dialog.Close";

/* ─── public namespace ──────────────────────────────────────────────── */

type DialogComponent = typeof DialogRoot & {
  Trigger: typeof DialogTrigger;
  Portal: typeof DialogPortal;
  Viewport: typeof DialogViewport;
  Backdrop: typeof DialogBackdrop;
  Popup: typeof DialogPopup;
  Header: typeof DialogHeader;
  Title: typeof DialogTitle;
  Description: typeof DialogDescription;
  Body: typeof DialogBody;
  Footer: typeof DialogFooter;
  Close: typeof DialogClose;
  createHandle: typeof createDialogHandle;
};

export const Dialog = DialogRoot as DialogComponent;
Dialog.Trigger = DialogTrigger;
Dialog.Portal = DialogPortal;
Dialog.Viewport = DialogViewport;
Dialog.Backdrop = DialogBackdrop;
Dialog.Popup = DialogPopup;
Dialog.Header = DialogHeader;
Dialog.Title = DialogTitle;
Dialog.Description = DialogDescription;
Dialog.Body = DialogBody;
Dialog.Footer = DialogFooter;
Dialog.Close = DialogClose;
Dialog.createHandle = createDialogHandle;
