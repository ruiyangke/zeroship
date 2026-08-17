import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ReactNode,
  type Ref,
} from "react";
import { Button, type ButtonProps } from "./Button";
import { cn } from "./cn";

export type DialogSize = "sm" | "md" | "lg" | "full";
export type DialogPlacement = "center" | "top";
export type DialogBackdropTint = "scrim" | "material" | "invisible";

type BaseRootProps = BaseDialog.Root.Props;
type OpenChangeDetails = Parameters<
  NonNullable<BaseRootProps["onOpenChange"]>
>[1];

export interface DialogProps extends Omit<
  BaseRootProps,
  "disablePointerDismissal"
> {
  dismissible?: boolean;
}

const DialogRoot = ({
  dismissible = true,
  modal = true,
  onOpenChange,
  ...props
}: DialogProps) => {
  const handleOpenChange = (open: boolean, details: OpenChangeDetails) => {
    if (
      !dismissible &&
      !open &&
      (details.reason === "escape-key" || details.reason === "outside-press")
    ) {
      details.cancel();
      return;
    }
    onOpenChange?.(open, details);
  };

  return (
    <BaseDialog.Root
      {...props}
      modal={modal}
      disablePointerDismissal={!dismissible}
      onOpenChange={handleOpenChange}
    />
  );
};

const DialogTrigger = forwardRef<HTMLElement, BaseDialog.Trigger.Props>(
  function DialogTrigger(props, ref) {
    return (
      <BaseDialog.Trigger
        {...props}
        ref={ref as Ref<HTMLButtonElement>}
      />
    );
  },
);

const DialogPortal = (props: BaseDialog.Portal.Props) => {
  return <BaseDialog.Portal {...props} />;
};

export interface DialogBackdropProps extends BaseDialog.Backdrop.Props {
  tint?: DialogBackdropTint;
}

const DialogBackdrop = forwardRef<HTMLDivElement, DialogBackdropProps>(
  function DialogBackdrop(
    { tint = "scrim", className, ...props },
    ref,
  ) {
    return (
      <BaseDialog.Backdrop
        {...props}
        ref={ref}
        className={(state) => {
          const consumerClasses =
            typeof className === "function" ? className(state) : className;
          return cn(consumerClasses) || undefined;
        }}
        data-slot="dialog-backdrop"
        data-tint={tint}
      />
    );
  },
);

export interface DialogPopupProps extends BaseDialog.Popup.Props {
  size?: DialogSize;
  placement?: DialogPlacement;
}

const DialogPopup = forwardRef<HTMLDivElement, DialogPopupProps>(
  function DialogPopup(
    { size = "md", placement = "center", className, ...props },
    ref,
  ) {
    return (
      <BaseDialog.Popup
        {...props}
        ref={ref}
        className={(state) => {
          const consumerClasses =
            typeof className === "function" ? className(state) : className;
          return cn(consumerClasses) || undefined;
        }}
        data-slot="dialog-popup"
        data-size={size}
        data-placement={placement}
      />
    );
  },
);

function CloseIcon() {
  return (
    <svg
      aria-hidden="true"
      data-size="sm"
      data-slot="icon"
      fill="none"
      focusable="false"
      stroke="currentColor"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="2"
      viewBox="0 0 24 24"
    >
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </svg>
  );
}

export interface DialogHeaderProps extends ComponentPropsWithoutRef<"div"> {
  showClose?: boolean;
  closeLabel?: string;
}

const DialogHeader = forwardRef<HTMLDivElement, DialogHeaderProps>(
  function DialogHeader(
    { showClose = true, closeLabel = "Close", children, ...props },
    ref,
  ) {
    return (
      <div {...props} ref={ref} data-slot="dialog-header">
        <div data-slot="dialog-header-content">{children}</div>
        {showClose ? (
          <BaseDialog.Close
            aria-label={closeLabel}
            data-slot="dialog-header-close"
          >
            <CloseIcon />
          </BaseDialog.Close>
        ) : null}
      </div>
    );
  },
);

const DialogTitle = forwardRef<
  HTMLHeadingElement,
  BaseDialog.Title.Props
>(function DialogTitle(props, ref) {
  return <BaseDialog.Title {...props} ref={ref} data-slot="dialog-title" />;
});

const DialogDescription = forwardRef<
  HTMLParagraphElement,
  BaseDialog.Description.Props
>(function DialogDescription(props, ref) {
  return (
    <BaseDialog.Description
      {...props}
      ref={ref}
      data-slot="dialog-description"
    />
  );
});

const DialogBody = forwardRef<
  HTMLDivElement,
  ComponentPropsWithoutRef<"div">
>(function DialogBody(props, ref) {
  return <div {...props} ref={ref} data-slot="dialog-body" />;
});

const DialogFooter = forwardRef<
  HTMLDivElement,
  ComponentPropsWithoutRef<"div">
>(function DialogFooter(props, ref) {
  return <div {...props} ref={ref} data-slot="dialog-footer" />;
});

export interface DialogCloseProps extends Omit<ButtonProps, "type"> {
  children?: ReactNode;
}

const DialogClose = forwardRef<HTMLButtonElement, DialogCloseProps>(
  function DialogClose(
    { variant = "gray", intent = "normal", children, ...props },
    ref,
  ) {
    return (
      <BaseDialog.Close
        {...props}
        ref={ref}
        render={<Button variant={variant} intent={intent} />}
      >
        {children}
      </BaseDialog.Close>
    );
  },
);

DialogTrigger.displayName = "Dialog.Trigger";
DialogBackdrop.displayName = "Dialog.Backdrop";
DialogPopup.displayName = "Dialog.Popup";
DialogHeader.displayName = "Dialog.Header";
DialogTitle.displayName = "Dialog.Title";
DialogDescription.displayName = "Dialog.Description";
DialogBody.displayName = "Dialog.Body";
DialogFooter.displayName = "Dialog.Footer";
DialogClose.displayName = "Dialog.Close";

export const Dialog = Object.assign(DialogRoot, {
  Trigger: DialogTrigger,
  Portal: DialogPortal,
  Backdrop: DialogBackdrop,
  Popup: DialogPopup,
  Header: DialogHeader,
  Title: DialogTitle,
  Description: DialogDescription,
  Body: DialogBody,
  Footer: DialogFooter,
  Close: DialogClose,
});
