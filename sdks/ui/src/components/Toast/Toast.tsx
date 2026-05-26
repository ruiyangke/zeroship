import {
  createContext,
  useContext,
  useEffect,
  useId,
  type HTMLAttributes,
  type ReactNode,
} from "react";
import { Toast as BaseToast } from "@base-ui/react/toast";
import clsx from "clsx";
import type { Tone } from "../Badge";

interface ToastData {
  action?: ReactNode;
}

const ToastScopeContext = createContext(false);

export interface ToastProps extends Omit<HTMLAttributes<HTMLDivElement>, "title"> {
  tone?: Tone;
  title?: ReactNode;
  action?: ReactNode;
}

export function Toast({
  tone = "neutral",
  title,
  action,
  children,
  className,
  ...props
}: ToastProps) {
  const managed = useContext(ToastScopeContext);
  if (managed) {
    return (
      <RegisteredToast tone={tone} title={title} action={action}>
        {children}
      </RegisteredToast>
    );
  }

  const role = tone === "danger" || tone === "warn" ? "alert" : "status";
  return (
    <div
      className={clsx("zs-toast", `zs-toast--${tone}`, className)}
      role={role}
      {...props}
    >
      <div className="zs-toast__content">
        {title && <div className="zs-toast__title">{title}</div>}
        {children && <div className="zs-toast__body">{children}</div>}
      </div>
      {action && <div className="zs-toast__action">{action}</div>}
    </div>
  );
}

function RegisteredToast({
  tone,
  title,
  action,
  children,
}: Required<Pick<ToastProps, "tone">> &
  Pick<ToastProps, "title" | "action" | "children">) {
  const id = useId();
  const { add, close } = BaseToast.useToastManager<ToastData>();

  useEffect(() => {
    add({
      id,
      title,
      description: children,
      type: tone,
      timeout: 0,
      priority: tone === "danger" || tone === "warn" ? "high" : "low",
      data: { action },
    });
    return () => close(id);
  }, [action, add, children, close, id, title, tone]);

  return null;
}

export interface ToastViewportProps extends HTMLAttributes<HTMLDivElement> {
  children?: ReactNode;
}

export function ToastViewport({ children, className, ...props }: ToastViewportProps) {
  return (
    <BaseToast.Provider timeout={0} limit={8}>
      <ToastScopeContext.Provider value>
        {children}
        <BaseToast.Portal>
          <BaseToast.Viewport
            className={clsx("zs-toast-viewport", className)}
            {...props}
          >
            <ToastList />
          </BaseToast.Viewport>
        </BaseToast.Portal>
      </ToastScopeContext.Provider>
    </BaseToast.Provider>
  );
}

function ToastList() {
  const manager = BaseToast.useToastManager<ToastData>();
  return (
    <>
      {manager.toasts.map((toast) => {
        const tone = toast.type ?? "neutral";
        return (
          <BaseToast.Root
            key={toast.id}
            toast={toast}
            className={clsx("zs-toast", `zs-toast--${tone}`)}
          >
            <div className="zs-toast__content">
              <BaseToast.Title className="zs-toast__title" />
              <BaseToast.Description className="zs-toast__body" />
            </div>
            {toast.data?.action && (
              <div className="zs-toast__action">{toast.data.action}</div>
            )}
            <BaseToast.Close className="zs-toast__close" aria-label="Dismiss">
              Dismiss
            </BaseToast.Close>
          </BaseToast.Root>
        );
      })}
    </>
  );
}

export const ToastParts = BaseToast;
