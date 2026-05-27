/*
 * Dialog — HIG Sheets / modal popup. Subparts mirror Base UI's headless
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
 * Slot order: spread `{...rest}` LAST so a consumer's `data-*` /
 * `aria-*` overrides win. Base UI handles every aria-wiring detail —
 * we never overwrite its attributes.
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
  cloneElement,
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import { Button, type ButtonProps } from "../Button";
import { composeRefs } from "../_slot";
import { classnames } from "../_classnames";

export type DialogSize = "sm" | "md" | "lg" | "full";
export type DialogPlacement = "center" | "top";
export type DialogBackdropTint = "scrim" | "material" | "none";

type BaseRootProps = ComponentPropsWithRef<typeof BaseDialog.Root>;
type BaseChangeEventDetails = Parameters<
  NonNullable<BaseRootProps["onOpenChange"]>
>[1];

export interface DialogProps {
  open?: boolean;
  defaultOpen?: boolean;
  /**
   * Called whenever the open state changes. Signature mirrors Base UI's
   * (open: boolean, eventDetails: { reason, event, cancel, ... }).
   *
   * The `reason` strings carried by `eventDetails` come from Base UI:
   * `'trigger-press' | 'outside-press' | 'escape-key' | 'close-press' |
   * 'focus-out' | 'imperative-action' | 'none'`. Cancel via
   * `eventDetails.cancel()` to veto the state change — that's how
   * `dismissible={false}` enforces non-dismissability for ESC.
   */
  onOpenChange?: (
    open: boolean,
    eventDetails: BaseChangeEventDetails,
  ) => void;
  /**
   * Modal behavior. `true` (default) traps focus, locks scroll,
   * disables pointer interaction outside. `false` or `'trap-focus'`
   * relax that — pass through to Base UI's `modal` prop.
   */
  modal?: boolean | "trap-focus";
  /**
   * Whether the dialog can be dismissed by outside-press OR Escape.
   * Default `true`. Setting `false` enforces non-dismissibility for
   * BOTH inputs (anti-pattern #12: never disable outside-click but
   * leave ESC). The user must use a Dialog.Close button to dismiss.
   */
  dismissible?: boolean;
  children?: ReactNode;
}

/* Compose our static class with a Base UI className that may be either
 * a string or a state-callback. Same shape as Field.tsx's
 * composeBaseClass invariant (slice 2 review fix 25). */
function composeBaseClass<S>(
  ours: string,
  theirs: string | ((state: S) => string | undefined) | undefined,
): string | ((state: S) => string | undefined) {
  if (theirs == null) return ours;
  if (typeof theirs === "string") return classnames(ours, theirs);
  return (state: S) => classnames(ours, theirs(state));
}

/* ─── Root ──────────────────────────────────────────────────────────── */

function DialogRoot({
  open,
  defaultOpen,
  onOpenChange,
  modal = true,
  dismissible = true,
  children,
}: DialogProps) {
  // `dismissible={false}` enforces non-dismissibility for BOTH
  // outside-press (via Base UI's `disablePointerDismissal`) AND
  // escape-key (via cancelling in the onOpenChange handler when the
  // reason is 'escape-key'). Anti-pattern #12: disabling outside-click
  // without ESC.
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
    <BaseDialog.Root
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={handleOpenChange}
      modal={modal}
      disablePointerDismissal={!dismissible}
    >
      {children}
    </BaseDialog.Root>
  );
}
DialogRoot.displayName = "Dialog";

/* ─── Trigger ───────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseDialog.Trigger>;
export type DialogTriggerProps = BaseTriggerProps;

const DialogTrigger = forwardRef<HTMLButtonElement, DialogTriggerProps>(
  function DialogTrigger({ className, ...rest }, ref) {
    return (
      <BaseDialog.Trigger
        ref={ref}
        className={composeBaseClass("zs-dialog-trigger", className)}
        {...rest}
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

/* ─── Backdrop ──────────────────────────────────────────────────────── */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BaseDialog.Backdrop>;
export interface DialogBackdropProps extends BaseBackdropProps {
  /**
   * Backdrop tint.
   * - `scrim` (default): a 40% black overlay — HIG-standard sheet
   *   backdrop.
   * - `material`: a translucent surface with backdrop-filter — for
   *   sheets layered over content-rich backgrounds (gallery views).
   * - `none`: no visual tint — for non-modal dialogs that need a
   *   click-blocker without the dimming effect.
   */
  tint?: DialogBackdropTint;
}

const DialogBackdrop = forwardRef<HTMLDivElement, DialogBackdropProps>(
  function DialogBackdrop({ tint = "scrim", className, ...rest }, ref) {
    return (
      <BaseDialog.Backdrop
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-dialog-backdrop", className)}
        data-tint={tint}
        {...rest}
      />
    );
  },
);
DialogBackdrop.displayName = "Dialog.Backdrop";

/* ─── Popup ─────────────────────────────────────────────────────────── */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseDialog.Popup>;
export interface DialogPopupProps extends BasePopupProps {
  /** Maximum inline-size of the popup. Default `md`. */
  size?: DialogSize;
  /** Vertical placement. Default `center`. */
  placement?: DialogPlacement;
}

const DialogPopup = forwardRef<HTMLDivElement, DialogPopupProps>(
  function DialogPopup(
    { size = "md", placement = "center", className, ...rest },
    ref,
  ) {
    return (
      <BaseDialog.Popup
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-dialog-popup", className)}
        data-size={size}
        data-placement={placement}
        {...rest}
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
            <CloseGlyph />
          </BaseDialog.Close>
        ) : null}
      </div>
    );
  },
);
DialogHeader.displayName = "Dialog.Header";

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
const DialogTitle = forwardRef<HTMLHeadingElement, BaseTitleProps>(
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
const DialogDescription = forwardRef<HTMLParagraphElement, BaseDescriptionProps>(
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

const DialogBody = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<"div">>(
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

const DialogFooter = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<"div">>(
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

export interface DialogCloseProps
  extends Omit<ButtonProps, "type"> {
  /**
   * Render as the single child element rather than our Button. Used
   * when the caller wants a custom button styling but the Base UI
   * close-on-press behavior.
   */
  asChild?: boolean;
}

/**
 * Dialog.Close — styled wrapper around our Button that participates in
 * Base UI's close-on-press machinery. Renders a <BaseDialog.Close>
 * with our Button inside via render-prop. Default variant is `gray`
 * (neutral cancel). Override with `variant="filled"` for primary
 * confirm buttons.
 */
const DialogClose = forwardRef<HTMLElement, DialogCloseProps>(
  function DialogClose(
    { asChild = false, variant = "gray", children, ...rest },
    ref,
  ) {
    return (
      <BaseDialog.Close
        nativeButton={false}
        render={(closeProps) => {
          if (asChild) {
            if (!isValidElement(children)) return <></>;
            const child = children as ReactElement<Record<string, unknown>> & {
              ref?: Ref<unknown>;
            };
            return cloneElement(child, {
              ...closeProps,
              ref: composeRefs(
                ref as Ref<unknown>,
                child.ref,
                (closeProps as { ref?: Ref<unknown> }).ref,
              ),
            } as Record<string, unknown>);
          }
          return (
            <Button
              {...closeProps}
              {...rest}
              ref={composeRefs(
                ref,
                (closeProps as { ref?: Ref<HTMLElement> }).ref,
              )}
              variant={variant}
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
  Backdrop: typeof DialogBackdrop;
  Popup: typeof DialogPopup;
  Header: typeof DialogHeader;
  Title: typeof DialogTitle;
  Description: typeof DialogDescription;
  Body: typeof DialogBody;
  Footer: typeof DialogFooter;
  Close: typeof DialogClose;
};

export const Dialog = DialogRoot as DialogComponent;
Dialog.Trigger = DialogTrigger;
Dialog.Portal = DialogPortal;
Dialog.Backdrop = DialogBackdrop;
Dialog.Popup = DialogPopup;
Dialog.Header = DialogHeader;
Dialog.Title = DialogTitle;
Dialog.Description = DialogDescription;
Dialog.Body = DialogBody;
Dialog.Footer = DialogFooter;
Dialog.Close = DialogClose;
