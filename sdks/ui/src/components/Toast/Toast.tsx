/*
 * Toast — transient notification surface.
 *
 *   <Toast.Provider>                       -- wrap once at app root
 *     <Toast.Viewport position="bottom-end" />
 *     {* The Viewport renders the toast stack itself; the manager
 *        emits new toasts from anywhere via the imperative
 *        useToast() hook. The compound parts below are for callers
 *        who need fully-custom rendering. *}
 *   </Toast.Provider>
 *
 *   // somewhere in a handler
 *   const { toast, dismiss } = useToast();
 *   toast.success({ title: "Saved", description: "Changes applied." });
 *   toast({ id: "upload", title: "Uploading…" });
 *   toast({ id: "upload", title: "Upload complete", variant: "success" });
 *   dismiss("upload");
 *
 * Shape decisions:
 *   - The imperative `useToast()` hook is the public, ergonomic surface.
 *     Compound parts (`Toast.{Provider,Viewport,Root,Title,Description,
 *     Action,Close,Portal}`) exist so consumers who need fully-custom
 *     rendering can opt into the structural anatomy. Most code never
 *     touches them.
 *   - Same-id `toast({id: "x"})` updates the existing toast in place
 *     (Base UI's `add` shape — when an id matches a live toast, it
 *     replaces the fields and resets the auto-dismiss timer). The
 *     update bumps `updateKey` so a single mount animates the swap
 *     instead of unmounting/remounting.
 *   - Roles + aria-live are wired explicitly per variant on `Toast.Root`,
 *     overriding Base UI's `role="dialog"` default. `default`, `info`,
 *     and `success` use `role="status"` + `aria-live="polite"`;
 *     `warning` uses `role="status"` + `aria-live="assertive"`; `error`
 *     uses `role="alert"` + `aria-live="assertive"`. Base UI's
 *     `useRenderElement` spreads `elementProps` after `defaultProps`,
 *     so the explicit `role`/`aria-live` props on `<BaseToast.Root>` win.
 *   - The toast `type` field carries our variant (Base UI exposes it as
 *     a string state attribute on every subpart, so CSS can tint titles,
 *     descriptions, action buttons, and leading icons off a single
 *     `[data-type="success"]` selector without React having to plumb the
 *     variant through context).
 *   - Variant → `priority` mapping seeds Base UI's invisible mirror for
 *     screen readers: `error` and `warning` map to `high` (assertive
 *     mirror, focus-redirect on `F6`); the rest map to `low`. Our
 *     explicit `role`/`aria-live` on Toast.Root is the primary path; the
 *     priority signal is the redundant secondary channel Base UI uses
 *     to keep its viewport semantics correct.
 *
 * Anti-patterns we explicitly avoid:
 *   - `position: fixed` on `Toast.Root` — Toast.Viewport owns
 *     positioning. Root paints relative to the Viewport stack so swipe
 *     and stack-collapse animations stay correct.
 *   - JSX-rendered Toasts as children of a Trigger button: the
 *     imperative `toast()` hook is the only entry point. The compound
 *     `Toast.Root` exists for the Viewport's render-prop loop, not for
 *     declarative mounting next to a trigger.
 *   - Silent dismissal on duplicate id: a second `toast({id: "x"})`
 *     UPDATES the existing toast (Base UI's documented contract). We
 *     surface this in `useToast` JSDoc.
 *   - Glass-surface invariant: `--zs-surface-raised` carries the panel
 *     opacity; Toast.Root paints an opaque background so axe's
 *     color-contrast walk terminates inside the toast.
 */
import {
  forwardRef,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import {
  Toast as BaseToast,
  type ToastRootToastObject,
} from "@base-ui/react/toast";
import { composeBaseClass, classnames } from "../_classnames";

/** Base UI's per-toast payload. The manager publishes this for each
 * live toast; the Viewport's default render loop walks the list and
 * passes each entry to Toast.Root. Re-exported so consumers writing a
 * custom Viewport render-prop typecheck against the same shape. */
export type ToastPayload = ToastRootToastObject<Record<string, unknown>>;

/* ─── Public shape types ────────────────────────────────────────────── */

/** Variant tone. Drives the leading-icon tint, the aria-live priority,
 * and the role exposed by Toast.Root. */
export type ToastVariant =
  | "default"
  | "success"
  | "error"
  | "warning"
  | "info";

/** Viewport corner. `top` and `bottom` center on the inline axis;
 * `top-start`, `top-end`, `bottom-start`, `bottom-end` anchor to the
 * logical inline ends (RTL-correct). */
export type ToastPosition =
  | "top-start"
  | "top-end"
  | "bottom-start"
  | "bottom-end"
  | "top"
  | "bottom";

/** Swipe axis Toast.Root accepts dismiss gestures along. `start`/`end`
 * map to logical inline ends; `up`/`down` map to block ends. */
export type ToastSwipeDirection = "start" | "end" | "up" | "down";

/* ─── Provider ──────────────────────────────────────────────────────── *
 *
 * Shares the toast queue across the whole app. Wrap once at the root.
 * Default `timeout` of 5000ms matches the brief; consumers can override
 * per-toast via `toast({ duration })`. */

type BaseProviderProps = ComponentPropsWithoutRef<typeof BaseToast.Provider>;

export interface ToastProviderProps
  extends Omit<BaseProviderProps, "timeout"> {
  /**
   * Default auto-dismiss duration in milliseconds. A toast with
   * `duration: 0` never auto-dismisses (manual `dismiss()` only).
   *
   * @default 5000
   */
  duration?: number;
  /**
   * Maximum number of toasts that can be visible at once. When the
   * limit is reached, the oldest toast is closed to make room. The
   * default matches Base UI's built-in cap.
   *
   * @default 3
   */
  limit?: number;
  /** Children render INSIDE the provider — usually a `<Toast.Viewport>`
   * plus the rest of your app. */
  children?: ReactNode;
}

function ToastProvider({
  duration = 5000,
  limit = 3,
  children,
  ...rest
}: ToastProviderProps) {
  return (
    <BaseToast.Provider
      timeout={duration}
      limit={limit}
      {...rest}
    >
      {children}
    </BaseToast.Provider>
  );
}
ToastProvider.displayName = "Toast.Provider";

/* ─── Viewport ──────────────────────────────────────────────────────── *
 *
 * Owns positioning. Renders a labelled live region (Base UI's defaults:
 * `role=region` + `aria-live=polite`) that contains the toast stack.
 * The Viewport itself is `position: fixed`; Toast.Root paints relative
 * to it so swipe/stack-collapse transforms stay correct.
 *
 * Default rendering — when the consumer passes no `children`, the
 * Viewport subscribes to the manager and renders each live toast with
 * our standard anatomy (leading icon ← in CSS via ::before, title,
 * description, action, close). The imperative `useToast()` hook is the
 * canonical surface; this default loop is what makes it "just work".
 *
 * Custom rendering — when the consumer passes `children`, that's the
 * render list. The compound `Toast.Root` + Title/Description/Action/
 * Close parts are intended for this path. */

type BaseViewportProps = ComponentPropsWithoutRef<typeof BaseToast.Viewport>;

export interface ToastViewportProps
  extends Omit<BaseViewportProps, "className"> {
  /**
   * Where the toast stack anchors inside the viewport. Logical-end
   * variants (`top-start`/`top-end`/`bottom-start`/`bottom-end`) flip
   * automatically under `direction: rtl`.
   *
   * @default "bottom-end"
   */
  position?: ToastPosition;
  /** Optional class hook on the viewport container. */
  className?: string;
  /**
   * Custom render list. Omit for the default loop — the Viewport
   * subscribes to the manager and renders each toast with the standard
   * anatomy. Pass children when you need fully-custom rendering; you
   * own the loop and must subscribe to `useToastManager()` yourself.
   */
  children?: ReactNode;
}

const ToastViewport = forwardRef<HTMLDivElement, ToastViewportProps>(
  function ToastViewport(
    { position = "bottom-end", className, children, ...rest },
    ref,
  ) {
    return (
      <BaseToast.Viewport
        ref={ref}
        {...rest}
        data-position={position}
        className={classnames(
          "zs-toast-viewport",
          `zs-toast-viewport--${position}`,
          className,
        )}
      >
        {children ?? <DefaultToastList />}
      </BaseToast.Viewport>
    );
  },
);
ToastViewport.displayName = "Toast.Viewport";

/** Default render loop for the Viewport. Subscribes to the manager,
 * walks the live toast list, and renders each entry with the standard
 * anatomy (leading icon paints via CSS ::before; Action mounts only
 * when the manager carries `actionProps`). */
function DefaultToastList() {
  const manager = BaseToast.useToastManager();
  return (
    <>
      {manager.toasts.map((entry) => (
        <ToastRoot key={entry.id} toast={entry}>
          <div className="zs-toast-content">
            {entry.title ? <ToastTitle>{entry.title}</ToastTitle> : null}
            {entry.description ? (
              <ToastDescription>{entry.description}</ToastDescription>
            ) : null}
            {entry.actionProps ? (
              <ToastAction {...entry.actionProps} />
            ) : null}
          </div>
          <ToastClose />
        </ToastRoot>
      ))}
    </>
  );
}

/* ─── Portal ────────────────────────────────────────────────────────── *
 *
 * Optional — the Viewport is already mounted into the document; Portal
 * is only useful when consumers want the Viewport itself rendered
 * inside a different DOM subtree (e.g. shadow root). Re-exported for
 * compound-API completeness. */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseToast.Portal>;
export type ToastPortalProps = BasePortalProps;

function ToastPortal(props: ToastPortalProps) {
  return <BaseToast.Portal {...props} />;
}
ToastPortal.displayName = "Toast.Portal";

/* ─── Root ──────────────────────────────────────────────────────────── *
 *
 * Renders a single toast in the Viewport stack. Receives a `toast`
 * payload (Base UI's `ToastObject<Data>`) emitted by the manager.
 *
 * `role` and `aria-live` are wired explicitly per variant:
 *
 *   default | info | success → role="status",  aria-live="polite"
 *   warning                  → role="status",  aria-live="assertive"
 *   error                    → role="alert",   aria-live="assertive"
 *
 * Base UI's default (`role="dialog"`) is appropriate for a toast that
 * carries an interactive Action, but the brief mandates `status`/`alert`
 * for live-region semantics; our explicit props override Base UI's
 * defaults.
 */

type BaseRootProps = ComponentPropsWithRef<typeof BaseToast.Root>;

export interface ToastRootProps
  extends Omit<BaseRootProps, "className" | "swipeDirection"> {
  /**
   * Direction(s) the toast can be swiped to dismiss. Logical `start`/
   * `end` map to the inline axis (correct under RTL); `up`/`down` map
   * to the block axis. Pass an array to enable multiple directions.
   *
   * @default ["end", "down"]
   */
  swipeDirection?: ToastSwipeDirection | ToastSwipeDirection[];
  /** Optional class hook on the toast root element. */
  className?: string;
}

function resolveSwipeDirection(
  swipe: ToastSwipeDirection | ToastSwipeDirection[] | undefined,
): BaseRootProps["swipeDirection"] {
  const map: Record<ToastSwipeDirection, "left" | "right" | "up" | "down"> = {
    start: "left",
    end: "right",
    up: "up",
    down: "down",
  };
  if (swipe === undefined) return ["right", "down"];
  if (Array.isArray(swipe)) return swipe.map((dir) => map[dir]);
  return map[swipe];
}

function resolveRole(variant: ToastVariant): "status" | "alert" {
  return variant === "error" ? "alert" : "status";
}

function resolveAriaLive(variant: ToastVariant): "polite" | "assertive" {
  return variant === "error" || variant === "warning"
    ? "assertive"
    : "polite";
}

const ToastRoot = forwardRef<HTMLDivElement, ToastRootProps>(
  function ToastRoot(
    { toast, swipeDirection, className, ...rest },
    ref,
  ) {
    // `toast.type` is our variant — set by useToast() when adding.
    // Default to "default" when omitted so role/aria-live still resolve.
    const variant = (toast.type as ToastVariant | undefined) ?? "default";
    return (
      <BaseToast.Root
        ref={ref}
        toast={toast}
        swipeDirection={resolveSwipeDirection(swipeDirection)}
        role={resolveRole(variant)}
        aria-live={resolveAriaLive(variant)}
        data-variant={variant}
        className={classnames(
          "zs-toast-root",
          `zs-toast-root--${variant}`,
          className,
        )}
        {...rest}
      />
    );
  },
);
ToastRoot.displayName = "Toast.Root";

/* ─── Title ─────────────────────────────────────────────────────────── *
 *
 * Renders Base UI's `<h2>` title element. Base UI registers the id with
 * the Root so the toast's `aria-labelledby` resolves here. */

type BaseTitleProps = ComponentPropsWithRef<typeof BaseToast.Title>;

export interface ToastTitleProps extends Omit<BaseTitleProps, "className"> {
  /** Optional class hook on the title element. */
  className?: string;
}

const ToastTitle = forwardRef<HTMLHeadingElement, ToastTitleProps>(
  function ToastTitle({ className, ...rest }, ref) {
    return (
      <BaseToast.Title
        ref={ref}
        className={composeBaseClass("zs-toast-title", className)}
        {...rest}
      />
    );
  },
);
ToastTitle.displayName = "Toast.Title";

/* ─── Description ───────────────────────────────────────────────────── *
 *
 * Renders Base UI's `<p>` description. Registers its id on the Root so
 * the toast's `aria-describedby` resolves here. */

type BaseDescriptionProps = ComponentPropsWithRef<typeof BaseToast.Description>;

export interface ToastDescriptionProps
  extends Omit<BaseDescriptionProps, "className"> {
  /** Optional class hook on the description element. */
  className?: string;
}

const ToastDescription = forwardRef<
  HTMLParagraphElement,
  ToastDescriptionProps
>(function ToastDescription({ className, ...rest }, ref) {
  return (
    <BaseToast.Description
      ref={ref}
      className={composeBaseClass("zs-toast-description", className)}
      {...rest}
    />
  );
});
ToastDescription.displayName = "Toast.Description";

/* ─── Action ────────────────────────────────────────────────────────── *
 *
 * A `<button>` that runs the consumer's callback and then dismisses the
 * toast. The dismissal is wired by Base UI — clicking the Action closes
 * the toast after the click handler resolves. */

type BaseActionProps = ComponentPropsWithRef<typeof BaseToast.Action>;

export interface ToastActionProps extends Omit<BaseActionProps, "className"> {
  /** Optional class hook on the action button. */
  className?: string;
}

const ToastAction = forwardRef<HTMLButtonElement, ToastActionProps>(
  function ToastAction({ className, ...rest }, ref) {
    return (
      <BaseToast.Action
        ref={ref}
        className={composeBaseClass("zs-toast-action", className)}
        {...rest}
      />
    );
  },
);
ToastAction.displayName = "Toast.Action";

/* ─── Close ─────────────────────────────────────────────────────────── *
 *
 * A `<button aria-label="Dismiss">` that closes the toast. The default
 * glyph is a small inline SVG `×`; consumers can pass children to
 * override (e.g. an icon component). */

type BaseCloseProps = ComponentPropsWithRef<typeof BaseToast.Close>;

export interface ToastCloseProps extends Omit<BaseCloseProps, "className"> {
  /** Optional class hook on the close button. */
  className?: string;
}

const ToastClose = forwardRef<HTMLButtonElement, ToastCloseProps>(
  function ToastClose({ className, children, ...rest }, ref) {
    return (
      <BaseToast.Close
        ref={ref}
        aria-label={rest["aria-label"] ?? "Dismiss notification"}
        className={composeBaseClass("zs-toast-close", className)}
        {...rest}
      >
        {children ?? <CloseGlyph />}
      </BaseToast.Close>
    );
  },
);
ToastClose.displayName = "Toast.Close";

function CloseGlyph() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 12 12"
      aria-hidden="true"
      focusable="false"
    >
      <path
        d="M 2,2 L 10,10 M 10,2 L 2,10"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
      />
    </svg>
  );
}

/* ─── public namespace ──────────────────────────────────────────────── */

export type ToastComponent = {
  Provider: typeof ToastProvider;
  Viewport: typeof ToastViewport;
  Portal: typeof ToastPortal;
  Root: typeof ToastRoot;
  Title: typeof ToastTitle;
  Description: typeof ToastDescription;
  Action: typeof ToastAction;
  Close: typeof ToastClose;
};

/**
 * Toast namespace. The imperative `useToast()` hook is the canonical
 * public surface; the compound parts on this object exist so consumers
 * who need fully-custom rendering can opt into the structural anatomy.
 */
export const Toast: ToastComponent = {
  Provider: ToastProvider,
  Viewport: ToastViewport,
  Portal: ToastPortal,
  Root: ToastRoot,
  Title: ToastTitle,
  Description: ToastDescription,
  Action: ToastAction,
  Close: ToastClose,
};
