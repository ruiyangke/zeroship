/*
 * AlertDialog — modal alert. A SEPARATE component from Dialog (Base UI,
 * Radix, shadcn, Chakra all converge on this), so the structural
 * differences are encoded in the shape itself:
 *
 *   - `role="alertdialog"` (vs Dialog's `role="dialog"`) — Base UI's
 *     Popup wires that automatically.
 *   - Non-dismissible by design — Base UI's AlertDialog Root omits
 *     `disablePointerDismissal`. Outside-press is ALWAYS ignored.
 *     ESC activates the Cancel button if present; no-op if absent
 *     (review-fix item 1). The wrapping `onOpenChange` intercepts the
 *     `'escape-key'` reason, cancels Base UI's close, and clicks the
 *     registered Cancel button so the composed onClick + close both run.
 *   - Smaller defaults — `size="sm"`, alert-tight radius, alert padding.
 *   - Footer auto-arranges children: 1 button → full-width; 2 buttons
 *     → side-by-side (Cancel left, Action right); 3+ → stacked
 *     vertically, destructive at the bottom (dev-warn enforces ordering,
 *     review-fix item 5).
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
 *   - HIG: destructive action without a Cancel — dev-warn (item 6).
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type MouseEvent as ReactMouseEvent,
  type MutableRefObject,
  type ReactNode,
  type Ref,
} from "react";
import { AlertDialog as BaseAlertDialog } from "@base-ui/react/alert-dialog";
import { Button, type ButtonProps } from "../Button";
import { Slot, composeRefs } from "../_slot";
import { classnames, composeBaseClass } from "../_classnames";

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

/* ─── Sentinel for footer child-walk (review-fix item 4) ────────────── */

/**
 * Static sentinel attached to the inner forwardRef components. Beats
 * the `displayName` lookup the Footer used to do — `displayName`
 * breaks the moment a consumer wraps the component in `React.memo` or
 * a thin wrapper. `React.memo` copies static properties onto its memo
 * shell, so the sentinel survives that path; we fall back to walking
 * `type?.type?.__zsAlertButton` for the memo case (item 4 contingency).
 */
type AlertButtonRole = "action" | "cancel";

function readAlertButtonRole(
  type: unknown,
): AlertButtonRole | undefined {
  if (!type || (typeof type !== "function" && typeof type !== "object")) {
    return undefined;
  }
  const direct = (type as { __zsAlertButton?: AlertButtonRole }).__zsAlertButton;
  if (direct === "action" || direct === "cancel") return direct;
  // memo(inner): `type` is the memo shell; `type.type` is the inner
  // forwardRef object. memo copies statics off the SHELL but for
  // belt-and-braces we walk one level deeper too.
  const inner = (type as { type?: { __zsAlertButton?: AlertButtonRole } }).type;
  const innerRole = inner?.__zsAlertButton;
  if (innerRole === "action" || innerRole === "cancel") return innerRole;
  return undefined;
}

/* ─── Cancel registration context (review-fix item 1) ───────────────── */

/**
 * Internal coupling between AlertDialog.Root and AlertDialog.Cancel. The
 * Root needs to know about (and click) the Cancel button when ESC fires
 * so the Cancel's composed onClick runs as part of the close path. The
 * Cancel registers a ref-holder on mount, unregisters on unmount. Last
 * register wins on the rare two-Cancel case (item 6's dev-warn fires
 * separately so the user notices).
 */
type CancelRefHolder = MutableRefObject<HTMLButtonElement | null>;

type AlertDialogContextValue = {
  registerCancel: (ref: CancelRefHolder) => void;
  unregisterCancel: (ref: CancelRefHolder) => void;
};

const AlertDialogContext = createContext<AlertDialogContextValue | null>(null);

/* ─── Root ──────────────────────────────────────────────────────────── */

function AlertDialogRootImpl({
  open,
  defaultOpen,
  onOpenChange,
  children,
}: AlertDialogProps) {
  // Stack of registered Cancel refs. Last-pushed is the active one; the
  // unregister call slices it back out so unmount ordering doesn't
  // strand a stale Cancel as "active".
  const registeredCancelsRef = useRef<CancelRefHolder[]>([]);

  const registerCancel = useCallback((ref: CancelRefHolder) => {
    registeredCancelsRef.current.push(ref);
  }, []);
  const unregisterCancel = useCallback((ref: CancelRefHolder) => {
    const list = registeredCancelsRef.current;
    const idx = list.lastIndexOf(ref);
    if (idx >= 0) list.splice(idx, 1);
  }, []);
  const ctxValue = useMemo<AlertDialogContextValue>(
    () => ({ registerCancel, unregisterCancel }),
    [registerCancel, unregisterCancel],
  );

  // Wrap onOpenChange. When the close request comes from `'escape-key'`,
  // we intercept: if there's an enabled Cancel registered, cancel Base
  // UI's close and click the Cancel (which runs the composed onClick
  // and then Base UI's close-press path). If NO Cancel is registered,
  // cancel the close — the alert becomes hard-modal: the user must
  // pick an Action. (See file-header note.)
  //
  // `eventDetails.cancel()` is the canonical Base UI 1.5 API
  // (`node_modules/.../createBaseUIEventDetails.d.ts` line 55).
  const handleOpenChange = (
    nextOpen: boolean,
    details: BaseChangeEventDetails,
  ) => {
    if (!nextOpen && details.reason === "escape-key") {
      // Walk the stack from the top (last-registered wins).
      const stack = registeredCancelsRef.current;
      let activeCancel: HTMLButtonElement | null = null;
      for (let i = stack.length - 1; i >= 0; i--) {
        const node = stack[i].current;
        if (node && !node.disabled) {
          activeCancel = node;
          break;
        }
      }
      if (activeCancel) {
        details.cancel();
        // click() triggers Cancel's composed onClick which in turn
        // runs Base UI's close-press handler — so the dialog closes
        // and the caller's onClick side-effects fire as one unit.
        activeCancel.click();
        return;
      }
      if (stack.length === 0) {
        // No Cancel at all → hard non-dismissible. The user must
        // choose an explicit Action.
        details.cancel();
        return;
      }
      // A Cancel exists but is disabled — also no-op (we don't want
      // ESC to bypass a disabled Cancel any more than a click would).
      details.cancel();
      return;
    }
    onOpenChange?.(nextOpen, details);
  };

  return (
    <AlertDialogContext.Provider value={ctxValue}>
      <BaseAlertDialog.Root
        open={open}
        defaultOpen={defaultOpen}
        onOpenChange={handleOpenChange}
      >
        {children}
      </BaseAlertDialog.Root>
    </AlertDialogContext.Provider>
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
        // The popup inherits `.zs-dialog-popup` forced-colors rules.
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
export type AlertDialogTitleProps = BaseTitleProps;

const AlertDialogTitle = forwardRef<HTMLHeadingElement, AlertDialogTitleProps>(
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
export type AlertDialogDescriptionProps = BaseDescriptionProps;

const AlertDialogDescription = forwardRef<HTMLParagraphElement, AlertDialogDescriptionProps>(
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

export type AlertDialogBodyProps = ComponentPropsWithoutRef<"div">;

const AlertDialogBody = forwardRef<HTMLDivElement, AlertDialogBodyProps>(
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

/**
 * Flatten footer children into a list of `{element, role}` for the
 * sentinel-driven counts (review-fix item 4). Descends into Fragments
 * and arrays; ignores null/undefined/boolean/string children. Only
 * elements carrying the `__zsAlertButton` sentinel (Cancel/Action,
 * including memo-wrapped variants) are counted.
 */
type FlattenedAlertButton = {
  role: AlertButtonRole;
  tone: AlertDialogActionTone;
  index: number;
};

function flattenAlertButtons(children: ReactNode): FlattenedAlertButton[] {
  const out: FlattenedAlertButton[] = [];
  let nextIndex = 0;
  const visit = (node: ReactNode) => {
    if (node == null || typeof node === "boolean") return;
    if (typeof node === "string" || typeof node === "number") return;
    if (Array.isArray(node)) {
      for (const child of node) visit(child);
      return;
    }
    if (!isValidElement(node)) return;
    // Fragment: recurse into its children.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const elementType: any = node.type;
    if (
      elementType === undefined ||
      // React.Fragment is a Symbol; identity check below covers it.
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (typeof elementType === "symbol" && elementType.toString().includes("react.fragment"))
    ) {
      visit((node.props as { children?: ReactNode }).children);
      return;
    }
    const role = readAlertButtonRole(elementType);
    if (role) {
      const tone =
        role === "action"
          ? ((node.props as { tone?: AlertDialogActionTone }).tone ??
              "normal")
          : "normal";
      out.push({ role, tone, index: nextIndex++ });
    }
    // Don't descend into non-Fragment elements; the contract is
    // <Footer><Cancel/><Action/></Footer> — nesting inside a custom
    // wrapper hides the buttons from layout anyway.
  };
  visit(children);
  return out;
}

const AlertDialogFooter = forwardRef<HTMLDivElement, AlertDialogFooterProps>(
  function AlertDialogFooter({ className, children, ...rest }, ref) {
    const flattened = flattenAlertButtons(children);
    const buttonCount: "1" | "2" | "3+" =
      flattened.length <= 1 ? "1" : flattened.length === 2 ? "2" : "3+";

    // Dev-warns (anti-pattern #13, HIG safe-exit, destructive-bottom).
    // All three live in a useEffect keyed by a normalized signature so
    // controlled alerts that re-render on internal state don't spam
    // (review-fix item 8).
    const signature = flattened
      .map((b) => `${b.role}:${b.tone}:${b.index}`)
      .join("|");

    useEffect(() => {
      if (process.env.NODE_ENV === "production") return;

      // 13: multiple primary (non-destructive) Actions.
      const primaryActions = flattened.filter(
        (b) => b.role === "action" && b.tone !== "destructive",
      );
      if (primaryActions.length > 1) {
        // eslint-disable-next-line no-console
        console.warn(
          "[AlertDialog] Footer contains multiple non-destructive " +
            "<AlertDialog.Action> children. An alert should have at " +
            "most one primary action — distinguish destructive " +
            'choices with `tone="destructive"`.',
        );
      }

      // 5: destructive-at-bottom ordering in 3+ button layouts.
      if (buttonCount === "3+") {
        const destructiveIdx = flattened.findIndex(
          (b) => b.role === "action" && b.tone === "destructive",
        );
        if (destructiveIdx >= 0 && destructiveIdx !== flattened.length - 1) {
          // eslint-disable-next-line no-console
          console.warn(
            `[AlertDialog] In a 3+ button alert, the destructive ` +
              `action should be last in source order. Found at index ` +
              `${destructiveIdx} of ${flattened.length}.`,
          );
        }
      }

      // 6: destructive without Cancel.
      const hasDestructive = flattened.some(
        (b) => b.role === "action" && b.tone === "destructive",
      );
      const hasCancel = flattened.some((b) => b.role === "cancel");
      if (hasDestructive && !hasCancel) {
        // eslint-disable-next-line no-console
        console.warn(
          "[AlertDialog] Destructive action without a Cancel button. " +
            "People need a clear safe exit — add <AlertDialog.Cancel> " +
            "to the footer.",
        );
      }
      // Effect re-runs only when the normalized footer shape changes;
      // `flattened` is stable for the same signature.
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [signature]);

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
   * close-press handler via Slot (review-fix item 7).
   */
  asChild?: boolean;
}

const AlertDialogCancel = forwardRef<HTMLElement, AlertDialogCancelProps>(
  function AlertDialogCancel(
    {
      asChild = false,
      variant = "gray",
      onClick: callerOnClick,
      children,
      ...rest
    },
    ref,
  ) {
    // Register the underlying button with the Root so ESC can route to
    // it (review-fix item 1). We hold the ref locally and surface it to
    // the registration context on mount.
    const cancelRef = useRef<HTMLButtonElement | null>(null);
    const ctx = useContext(AlertDialogContext);
    useEffect(() => {
      if (!ctx) return undefined;
      ctx.registerCancel(cancelRef);
      return () => ctx.unregisterCancel(cancelRef);
    }, [ctx]);

    // Detect whether the asChild target is a native `<button>` so we
    // can drive `nativeButton` correctly (review-fix item 3). Mirrors
    // Dialog.Close's derivation.
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
        "AlertDialog.Cancel asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    return (
      <BaseAlertDialog.Close
        nativeButton={nativeButton}
        render={(closeProps) => {
          const closePropsRef = (closeProps as { ref?: Ref<unknown> }).ref;
          const closePropsOnClick = (
            closeProps as {
              onClick?: (event: ReactMouseEvent<HTMLElement>) => void;
            }
          ).onClick;

          // Composed onClick: caller runs first; if they don't
          // preventDefault, Base UI's close handler runs (review-fix
          // item 2 — twin of the Dialog.Close fix).
          const composedOnClick = (event: ReactMouseEvent<HTMLElement>) => {
            callerOnClick?.(event as ReactMouseEvent<HTMLButtonElement>);
            if (!event.defaultPrevented) {
              closePropsOnClick?.(event);
            }
          };

          if (asChild) {
            if (!isValidElement(children)) return <></>;
            // Slot's mergeProps composes the child's onClick with the
            // one we hand it (theirs first; if not preventDefault'd,
            // ours runs). We pass `composedOnClick` — caller's onClick
            // + Base UI close — and let Slot handle the child's
            // onClick. Manually extracting & calling the child's
            // onClick AGAIN here double-fires it (Slot still composes).
            // Mirrors Dialog.Close 3a64a726.
            return (
              <Slot
                {...closeProps}
                {...rest}
                ref={composeRefs(
                  ref as Ref<unknown>,
                  cancelRef as Ref<unknown>,
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
                cancelRef as Ref<HTMLElement>,
                closePropsRef as Ref<HTMLElement>,
              )}
              variant={variant}
              onClick={composedOnClick}
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

export interface AlertDialogActionProps
  extends Omit<ButtonProps, "type" | "intent"> {
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
    // activation closes the dialog. `data-tone` is emitted on BOTH
    // branches (review-fix item 5).
    if (preventClose) {
      return (
        <Button
          {...rest}
          ref={ref}
          variant={variant}
          intent={tone === "destructive" ? "destructive" : "normal"}
          onClick={callerOnClick}
          data-tone={tone}
        />
      );
    }
    return (
      <BaseAlertDialog.Close
        nativeButton
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
              // Run caller's onClick first; if they don't
              // preventDefault, Base UI's close handler runs. Same
              // composedOnClick shape as Cancel + Dialog.Close.
              callerOnClick?.(event);
              if (!event.defaultPrevented) {
                const closeHandler = (
                  closeProps as {
                    onClick?: (e: typeof event) => void;
                  }
                ).onClick;
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

/* Attach sentinels for the Footer's child-walk (review-fix item 4).
 * memo wrappers copy static properties off the inner forwardRef, so
 * the sentinel survives that path. */
(AlertDialogAction as unknown as { __zsAlertButton: AlertButtonRole }).__zsAlertButton =
  "action";
(AlertDialogCancel as unknown as { __zsAlertButton: AlertButtonRole }).__zsAlertButton =
  "cancel";

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
