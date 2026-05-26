import { type ReactNode } from "react";
import { Dialog as BaseDialog } from "@base-ui/react/dialog";
import clsx from "clsx";

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  className?: string;
}

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  className,
}: DialogProps) {
  return (
    <BaseDialog.Root open={open} onOpenChange={(next) => onOpenChange(next)}>
      <BaseDialog.Portal>
        <BaseDialog.Backdrop className="zs-dialog__backdrop" />
        <BaseDialog.Viewport className="zs-dialog__viewport">
          <BaseDialog.Popup className={clsx("zs-dialog__panel", className)}>
            <div className="zs-dialog__header">
              <div>
                <BaseDialog.Title className="zs-dialog__title">{title}</BaseDialog.Title>
                {description && (
                  <BaseDialog.Description className="zs-dialog__description">
                    {description}
                  </BaseDialog.Description>
                )}
              </div>
              <BaseDialog.Close className="zs-dialog__close" aria-label="Close dialog">
                Close
              </BaseDialog.Close>
            </div>
            <div className="zs-dialog__body">{children}</div>
            {footer && <div className="zs-dialog__footer">{footer}</div>}
          </BaseDialog.Popup>
        </BaseDialog.Viewport>
      </BaseDialog.Portal>
    </BaseDialog.Root>
  );
}

export const DialogParts = BaseDialog;
