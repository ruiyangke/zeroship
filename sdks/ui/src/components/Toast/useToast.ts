/*
 * useToast — imperative hook for emitting + dismissing toasts.
 *
 *   const { toast, dismiss } = useToast();
 *
 *   toast({ title: "Saved" });
 *   toast.success({ title: "Saved", description: "Changes applied." });
 *   toast.error({ title: "Upload failed", action: { label: "Retry", onClick } });
 *   toast.warning({ title: "Storage almost full" });
 *   toast.info({ title: "New build available" });
 *
 *   // Same id updates the existing toast in place (no second mount,
 *   // no flicker — Base UI bumps the toast's updateKey and resets
 *   // the auto-dismiss timer).
 *   const id = toast({ id: "upload", title: "Uploading…" });
 *   toast({ id, title: "Upload complete", variant: "success" });
 *
 *   // Manual dismissal. Pass undefined / no arg to clear ALL toasts.
 *   dismiss(id);
 *   dismiss();
 *
 * Notes:
 *   - `toast()` returns the toast id (the one you passed, or the
 *     auto-generated one Base UI minted). Hold onto it for later
 *     updates or manual dismissal.
 *   - The hook must be called inside a `<Toast.Provider>` subtree. Calling
 *     outside throws Base UI's manager-not-found error — a developer-
 *     time signal that the Provider wasn't mounted.
 *   - The hook surface is intentionally tiny (`toast`, `dismiss`). For
 *     fully-custom toast rendering (e.g. icon strips, custom anchor
 *     toasts), use the compound `Toast.{Provider,Viewport,Root,...}`
 *     parts directly — they share the same manager.
 */
import { useMemo } from "react";
import { Toast as BaseToast } from "@base-ui/react/toast";
import type { ToastVariant } from "./Toast";

/**
 * Options for emitting a single toast. Mirrors the brief's contract;
 * the manager's update operation accepts the same shape (minus `id`,
 * which is the key passed to `dismiss()`).
 */
export interface ToastOptions {
  /**
   * Stable identifier. Passing the same id twice UPDATES the existing
   * toast in place (no second mount); the auto-dismiss timer resets.
   * When omitted, Base UI mints a unique id.
   */
  id?: string;
  /** Headline text or node. Wired to the toast's `aria-labelledby`. */
  title?: React.ReactNode;
  /** Supplementary text or node. Wired to `aria-describedby`. */
  description?: React.ReactNode;
  /**
   * Optional action button. Clicking the button runs `onClick`
   * AND dismisses the toast.
   */
  action?: { label: string; onClick: () => void };
  /**
   * Auto-dismiss duration in milliseconds. `0` makes the toast
   * persistent (manual dismissal only). When omitted, the
   * `<Toast.Provider>`'s `duration` wins.
   *
   * @default <Toast.Provider duration> (5000)
   */
  duration?: number;
  /**
   * Visual variant. Drives:
   *   - the leading-icon tint (`--zs-system-{red,green,orange}` /
   *     `--zs-accent`),
   *   - the `role` on Toast.Root (`status` for default/info/success/
   *     warning, `alert` for error),
   *   - the `aria-live` on Toast.Root (`polite` for default/info/
   *     success, `assertive` for warning/error),
   *   - the visually-hidden mirror Base UI emits for screen readers
   *     (high-priority for error/warning, low-priority for the rest).
   *
   * @default "default"
   */
  variant?: ToastVariant;
  /**
   * Callback fired when the toast finishes its exit transition and
   * leaves the queue. Useful for cleaning up associated state when
   * the user dismisses (manually or via auto-timeout).
   */
  onClose?: () => void;
}

/** Return shape of the `useToast()` hook. */
export interface UseToastReturn {
  /**
   * Emit a toast. Returns the toast id. Includes shortcut methods
   * for the four named variants: `toast.success(...)`,
   * `toast.error(...)`, `toast.warning(...)`, `toast.info(...)`.
   */
  toast: ToastEmitter;
  /**
   * Dismiss a toast by id. Call with no arguments (or `undefined`)
   * to dismiss every active toast.
   */
  dismiss: (id?: string) => void;
}

/**
 * Imperative emitter for toasts. Calling it returns the id of the
 * emitted toast; the same id passed again updates the existing toast.
 * The variant shortcuts (`.success`, `.error`, `.warning`, `.info`)
 * fix `variant` so callers don't have to spell it out for the common
 * cases.
 */
export type ToastEmitter = ((options: ToastOptions) => string) & {
  /** Shortcut for `toast({ variant: "success", ... })`. */
  success: (options: Omit<ToastOptions, "variant">) => string;
  /** Shortcut for `toast({ variant: "error", ... })`. */
  error: (options: Omit<ToastOptions, "variant">) => string;
  /** Shortcut for `toast({ variant: "warning", ... })`. */
  warning: (options: Omit<ToastOptions, "variant">) => string;
  /** Shortcut for `toast({ variant: "info", ... })`. */
  info: (options: Omit<ToastOptions, "variant">) => string;
};

/** Variants that signal urgency raise the screen-reader priority so
 * Base UI's invisible mirror announces them immediately even when the
 * Viewport is not focused. The visible `aria-live` on Toast.Root is
 * set separately by ToastRoot itself. */
function priorityForVariant(variant: ToastVariant): "low" | "high" {
  return variant === "error" || variant === "warning" ? "high" : "low";
}

/** Monotonic counter for auto-generated toast ids. We need to allocate
 * the id BEFORE handing it to Base UI so the action wrapper closure
 * can call `manager.close(id)`; reusing the id Base UI mints internally
 * would require a second `add` round-trip. The counter is sufficient
 * for in-process uniqueness — toasts are short-lived UI state, not
 * cross-tab durable. */
let toastIdCounter = 0;
function generateToastId(): string {
  toastIdCounter += 1;
  return `zs-toast-${toastIdCounter}`;
}

/**
 * Subscribe to the Toast manager and return the canonical imperative
 * surface (`toast`, `dismiss`). Throws if called outside a
 * `<Toast.Provider>` — that's intentional (a developer-time signal
 * that the provider is missing from the tree).
 */
export function useToast(): UseToastReturn {
  const manager = BaseToast.useToastManager();

  return useMemo<UseToastReturn>(() => {
    const emit = (options: ToastOptions): string => {
      const {
        id,
        title,
        description,
        action,
        duration,
        variant = "default",
        onClose,
      } = options;
      // Allocate the toast id up front so the action's click handler
      // can call `manager.close(id)` after running the user callback —
      // Base UI's ToastAction does NOT auto-dismiss (it just runs the
      // configured `onClick`), so we wire the dismissal here. This
      // satisfies the brief's "Action button click runs callback AND
      // dismisses" contract.
      const toastId = id ?? generateToastId();
      const actionProps = action
        ? {
            children: action.label,
            onClick: () => {
              try {
                action.onClick();
              } finally {
                manager.close(toastId);
              }
            },
          }
        : undefined;
      return manager.add({
        id: toastId,
        title,
        description,
        // `type` is Base UI's variant channel — surfaced as
        // `data-type="<variant>"` on every subpart so CSS scopes off it.
        type: variant,
        priority: priorityForVariant(variant),
        timeout: duration,
        actionProps,
        onClose,
      });
    };

    const dismiss = (id?: string): void => {
      manager.close(id);
    };

    const toastFn = emit as ToastEmitter;
    toastFn.success = (options) => emit({ ...options, variant: "success" });
    toastFn.error = (options) => emit({ ...options, variant: "error" });
    toastFn.warning = (options) => emit({ ...options, variant: "warning" });
    toastFn.info = (options) => emit({ ...options, variant: "info" });

    return { toast: toastFn, dismiss };
  }, [manager]);
}
