/*
 * AlertDialog — HIG Alerts. A SEPARATE component from Dialog (Base UI,
 * Radix, shadcn, Chakra all converge on this), so the structural
 * differences are encoded in the shape itself:
 *
 *   - `role="alertdialog"` (vs Dialog's `role="dialog"`) — Base UI's
 *     Popup wires that automatically.
 *   - Non-dismissible by design — Base UI's AlertDialog Root omits
 *     `disablePointerDismissal`. Outside-press is ALWAYS ignored.
 *     ESC closes the Cancel button if present, otherwise dismisses
 *     (the safest default we can offer without forbidding ESC, which
 *     would trap the user's keyboard).
 *   - Smaller defaults — `size="sm"`, alert-tight radius, alert padding.
 *   - Footer auto-arranges children: 1 button → full-width; 2 buttons
 *     → side-by-side (Cancel left, Action right); 3+ → stacked
 *     vertically, destructive at the bottom.
 *   - `Cancel` / `Action` subparts are styled Buttons that auto-close
 *     the alert on activation.
 *
 * Anti-patterns we explicitly avoid:
 *   - 5: No role distinction between alert and regular. We're the
 *     dedicated alert path; Dialog is the regular path.
 *   - 6: AlertDialog dismissible by outside-click. Base UI's
 *     AlertDialogRoot enforces this by omitting the prop.
 *   - 13: Multiple primary actions. Dev-warn if Footer carries more
 *     than one AlertDialog.Action with no destructive distinction.
 */
import {
  Children,
  cloneElement,
  forwardRef,
  isValidElement,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { AlertDialog as BaseAlertDialog } from "@base-ui/react/alert-dialog";
import { Button, type ButtonProps } from "../Button";
import { composeRefs } from "../_slot";
import { classnames } from "../_classnames";

export type AlertDialogSize = "sm" | "md" | "lg";
export type AlertDialogActionTone = "normal" | "destructive";

type BaseRootProps = ComponentPropsWithRef<typeof BaseAlertDialog.Root>;
type BaseChangeEventDetails = Parameters<
  NonNullable<BaseRootProps["onOpenChange"]>
>[1];

export interface AlertDialogProps {
  open?: boolean;
  defaultOpen?: boolean;
  /**
   * Signature mirrors Base UI's. The `eventDetails.reason` will be one
   * of: `'trigger-press' | 'escape-key' | 'close-press' | 'focus-out'
   * | 'imperative-action' | 'none'`. Note: `'outside-press'` never
   * fires here — AlertDialog ignores outside-click by Base UI design.
   */
  onOpenChange?: (
    open: boolean,
    eventDetails: BaseChangeEventDetails,
  ) => void;
  children?: ReactNode;
}

function composeBaseClass<S>(
  ours: string,
  theirs: string | ((state: S) => string | undefined) | undefined,
): string | ((state: S) => string | undefined) {
  if (theirs == null) return ours;
  if (typeof theirs === "string") return classnames(ours, theirs);
  return (state: S) => classnames(ours, theirs(state));
}

/* ─── Root ──────────────────────────────────────────────────────────── */

function AlertDialogRootImpl({
  open,
  defaultOpen,
  onOpenChange,
  children,
}: AlertDialogProps) {
  return (
    <BaseAlertDialog.Root
      open={open}
      defaultOpen={defaultOpen}
      onOpenChange={onOpenChange}
    >
      {children}
    </BaseAlertDialog.Root>
  );
}
AlertDialogRootImpl.displayName = "AlertDialog";

/* ─── Trigger ───────────────────────────────────────────────────────── */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Trigger>;
export type AlertDialogTriggerProps = BaseTriggerProps;

const AlertDialogTrigger = forwardRef<HTMLButtonElement, AlertDialogTriggerProps>(
  function AlertDialogTrigger({ className, ...rest }, ref) {
    return (
      <BaseAlertDialog.Trigger
        ref={ref}
        className={composeBaseClass("zs-alertdialog-trigger", className)}
        {...rest}
      />
    );
  },
);
AlertDialogTrigger.displayName = "AlertDialog.Trigger";

/* ─── Portal / Backdrop ─────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Portal>;
export type AlertDialogPortalProps = BasePortalProps;

function AlertDialogPortal(props: AlertDialogPortalProps) {
  return <BaseAlertDialog.Portal {...props} />;
}
AlertDialogPortal.displayName = "AlertDialog.Portal";

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Backdrop>;
export type AlertDialogBackdropProps = BaseBackdropProps;

const AlertDialogBackdrop = forwardRef<HTMLDivElement, AlertDialogBackdropProps>(
  function AlertDialogBackdrop({ className, ...rest }, ref) {
    return (
      <BaseAlertDialog.Backdrop
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass(
          "zs-dialog-backdrop zs-alertdialog-backdrop",
          className,
        )}
        // Alerts use the canonical scrim — no `tint` prop offered.
        data-tint="scrim"
        {...rest}
      />
    );
  },
);
AlertDialogBackdrop.displayName = "AlertDialog.Backdrop";

/* ─── Popup ─────────────────────────────────────────────────────────── */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Popup>;
export interface AlertDialogPopupProps extends BasePopupProps {
  /** Max-width preset. Default `sm`. Most alerts are small. */
  size?: AlertDialogSize;
}

const AlertDialogPopup = forwardRef<HTMLDivElement, AlertDialogPopupProps>(
  function AlertDialogPopup({ size = "sm", className, ...rest }, ref) {
    return (
      <BaseAlertDialog.Popup
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass(
          "zs-dialog-popup zs-alertdialog-popup",
          className,
        )}
        data-size={size}
        {...rest}
      />
    );
  },
);
AlertDialogPopup.displayName = "AlertDialog.Popup";

/* ─── Header / Title / Description / Body ───────────────────────────── */

export interface AlertDialogHeaderProps
  extends ComponentPropsWithoutRef<"div"> {}

const AlertDialogHeader = forwardRef<HTMLDivElement, AlertDialogHeaderProps>(
  function AlertDialogHeader({ className, children, ...rest }, ref) {
    // No close button in alerts (anti-pattern #6 + alerts demand a
    // deliberate action choice from the Footer). The Header is purely
    // Title + Description.
    return (
      <div
        ref={ref}
        className={classnames(
          "zs-dialog__header zs-alertdialog__header",
          className,
        )}
        {...rest}
      >
        <div className="zs-dialog__header-content">{children}</div>
      </div>
    );
  },
);
AlertDialogHeader.displayName = "AlertDialog.Header";

type BaseTitleProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Title>;
const AlertDialogTitle = forwardRef<HTMLHeadingElement, BaseTitleProps>(
  function AlertDialogTitle({ className, ...rest }, ref) {
    return (
      <BaseAlertDialog.Title
        ref={ref}
        className={composeBaseClass("zs-dialog__title", className)}
        {...rest}
      />
    );
  },
);
AlertDialogTitle.displayName = "AlertDialog.Title";

type BaseDescriptionProps = ComponentPropsWithoutRef<typeof BaseAlertDialog.Description>;
const AlertDialogDescription = forwardRef<HTMLParagraphElement, BaseDescriptionProps>(
  function AlertDialogDescription({ className, ...rest }, ref) {
    return (
      <BaseAlertDialog.Description
        ref={ref}
        className={composeBaseClass("zs-dialog__description", className)}
        {...rest}
      />
    );
  },
);
AlertDialogDescription.displayName = "AlertDialog.Description";

const AlertDialogBody = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<"div">>(
  function AlertDialogBody({ className, ...rest }, ref) {
    return (
      <div
        ref={ref}
        className={classnames(
          "zs-dialog__body zs-alertdialog__body",
          className,
        )}
        {...rest}
      />
    );
  },
);
AlertDialogBody.displayName = "AlertDialog.Body";

/* ─── Footer ────────────────────────────────────────────────────────── */

export interface AlertDialogFooterProps
  extends ComponentPropsWithoutRef<"div"> {}

function countButtonChildren(children: ReactNode): "1" | "2" | "3+" {
  const count = Children.count(children);
  if (count <= 1) return "1";
  if (count === 2) return "2";
  return "3+";
}

const AlertDialogFooter = forwardRef<HTMLDivElement, AlertDialogFooterProps>(
  function AlertDialogFooter({ className, children, ...rest }, ref) {
    const buttonCount = countButtonChildren(children);

    // Dev-mode warning for "multiple primary actions" (anti-pattern #13).
    // We approximate: count children that are <AlertDialog.Action> and
    // do NOT carry `tone="destructive"`. If more than one such child,
    // warn — alerts should have ONE primary action.
    if (
      typeof process !== "undefined" &&
      process.env?.NODE_ENV !== "production"
    ) {
      let primaryActions = 0;
      Children.forEach(children, (child) => {
        if (
          isValidElement(child) &&
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          (child.type as any)?.displayName === "AlertDialog.Action"
        ) {
          const tone = (child.props as { tone?: AlertDialogActionTone }).tone;
          if (tone !== "destructive") primaryActions += 1;
        }
      });
      if (primaryActions > 1) {
        // eslint-disable-next-line no-console
        console.warn(
          "[AlertDialog] Footer contains multiple non-destructive " +
            "<AlertDialog.Action> children. Per HIG, an alert should " +
            "have at most one primary action — distinguish destructive " +
            'choices with `tone="destructive"`.',
        );
      }
    }

    return (
      <div
        ref={ref}
        className={classnames(
          "zs-dialog__footer zs-alertdialog__footer",
          className,
        )}
        data-button-count={buttonCount}
        {...rest}
      >
        {children}
      </div>
    );
  },
);
AlertDialogFooter.displayName = "AlertDialog.Footer";

/* ─── Cancel / Action ──────────────────────────────────────────────── */

export interface AlertDialogCancelProps extends Omit<ButtonProps, "type"> {
  /**
   * Render-as the single child element. Cancel still participates in
   * Base UI's close-on-press machinery — the child receives the
   * close-press handler via Slot.
   */
  asChild?: boolean;
}

const AlertDialogCancel = forwardRef<HTMLElement, AlertDialogCancelProps>(
  function AlertDialogCancel(
    { asChild = false, variant = "gray", children, ...rest },
    ref,
  ) {
    return (
      <BaseAlertDialog.Close
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
AlertDialogCancel.displayName = "AlertDialog.Cancel";

export interface AlertDialogActionProps extends Omit<ButtonProps, "type" | "intent"> {
  /**
   * Action tone. `destructive` flips the underlying Button to
   * `intent="destructive"` — the red-system action. Default `normal`.
   */
  tone?: AlertDialogActionTone;
  /**
   * If true, do NOT auto-close the AlertDialog when the action is
   * pressed. Used for action buttons that perform async work and the
   * caller wants to close manually after completion. Default false.
   */
  preventClose?: boolean;
}

const AlertDialogAction = forwardRef<HTMLButtonElement, AlertDialogActionProps>(
  function AlertDialogAction(
    {
      tone = "normal",
      preventClose = false,
      variant = "filled",
      onClick: callerOnClick,
      ...rest
    },
    ref,
  ) {
    // When `preventClose` is true, render a normal Button (no Base UI
    // close wrapper). When false, wrap in BaseAlertDialog.Close so
    // activation closes the dialog.
    if (preventClose) {
      return (
        <Button
          {...rest}
          ref={ref}
          variant={variant}
          intent={tone === "destructive" ? "destructive" : "normal"}
          onClick={callerOnClick}
        />
      );
    }
    return (
      <BaseAlertDialog.Close
        nativeButton={false}
        render={(closeProps) => (
          <Button
            {...closeProps}
            {...rest}
            ref={composeRefs(
              ref,
              (closeProps as { ref?: Ref<HTMLButtonElement> }).ref,
            )}
            variant={variant}
            intent={tone === "destructive" ? "destructive" : "normal"}
            onClick={(event) => {
              // Run caller's onClick first; Base UI's close handler
              // is what closeProps.onClick wires. Spread order in
              // Button puts closeProps.onClick AFTER our local onClick
              // — but we want close after caller runs. Re-implement
              // the merge here so the order is deterministic.
              callerOnClick?.(event);
              if (!event.defaultPrevented) {
                const closeHandler = (closeProps as { onClick?: (e: typeof event) => void }).onClick;
                closeHandler?.(event);
              }
            }}
            data-tone={tone}
          />
        )}
      />
    );
  },
);
AlertDialogAction.displayName = "AlertDialog.Action";

/* Re-export the displayName references the Footer's child-walk uses.
 * Without these explicit assignments the dev-warning lookup is brittle
 * to TS minification — the assignments above happen at module load
 * which is before the Footer is rendered. */

/* ─── public namespace ──────────────────────────────────────────────── */

type AlertDialogComponent = typeof AlertDialogRootImpl & {
  Trigger: typeof AlertDialogTrigger;
  Portal: typeof AlertDialogPortal;
  Backdrop: typeof AlertDialogBackdrop;
  Popup: typeof AlertDialogPopup;
  Header: typeof AlertDialogHeader;
  Title: typeof AlertDialogTitle;
  Description: typeof AlertDialogDescription;
  Body: typeof AlertDialogBody;
  Footer: typeof AlertDialogFooter;
  Cancel: typeof AlertDialogCancel;
  Action: typeof AlertDialogAction;
};

export const AlertDialog = AlertDialogRootImpl as AlertDialogComponent;
AlertDialog.Trigger = AlertDialogTrigger;
AlertDialog.Portal = AlertDialogPortal;
AlertDialog.Backdrop = AlertDialogBackdrop;
AlertDialog.Popup = AlertDialogPopup;
AlertDialog.Header = AlertDialogHeader;
AlertDialog.Title = AlertDialogTitle;
AlertDialog.Description = AlertDialogDescription;
AlertDialog.Body = AlertDialogBody;
AlertDialog.Footer = AlertDialogFooter;
AlertDialog.Cancel = AlertDialogCancel;
AlertDialog.Action = AlertDialogAction;
