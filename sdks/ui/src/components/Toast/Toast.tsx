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
  createContext,
  forwardRef,
  useContext,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
} from "react";
import {
  Toast as BaseToast,
  type ToastRootToastObject,
} from "@base-ui/react/toast";
import { useDirection } from "@base-ui/react/direction-provider";
import { X } from "lucide-react";
import { composeBaseClass, classnames } from "../_classnames";
import { Icon } from "../Icon";

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

/** Config the Provider broadcasts to its descendants — `DefaultToastList`
 * reads this so a global `swipeDirection` on the Provider applies to
 * every default-rendered toast without the caller wiring it on each
 * `Toast.Root`. */
interface ToastConfigContextValue {
  /** Global swipe direction the default render loop forwards to every
   * Toast.Root that doesn't carry an explicit `swipeDirection`. */
  swipeDirection: ToastSwipeDirection | ToastSwipeDirection[] | undefined;
}

const ToastConfigContext = createContext<ToastConfigContextValue>({
  swipeDirection: undefined,
});

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
  /**
   * Global swipe direction the default render loop applies to every
   * toast. Per-toast Root overrides via `<Toast.Root swipeDirection>`
   * still win when consumers go through the compound API.
   *
   * Logical `start`/`end` map to the inline axis (correct under RTL);
   * `up`/`down` map to the block axis. Pass an array for multi-axis
   * gestures.
   *
   * @default ["end", "down"]
   */
  swipeDirection?: ToastSwipeDirection | ToastSwipeDirection[];
  /** Children render INSIDE the provider — usually a `<Toast.Viewport>`
   * plus the rest of your app. */
  children?: ReactNode;
}

function ToastProvider({
  duration = 5000,
  limit = 3,
  swipeDirection,
  children,
  ...rest
}: ToastProviderProps) {
  return (
    <BaseToast.Provider
      timeout={duration}
      limit={limit}
      {...rest}
    >
      <ToastConfigContext.Provider value={{ swipeDirection }}>
        {children}
      </ToastConfigContext.Provider>
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
 * when the manager carries `actionProps`).
 *
 * Action is rendered as a bare `<ToastAction />` — Base UI's
 * `ToastAction` pulls `children` + `onClick` off the per-toast
 * `actionProps` payload via root context, so spreading them again here
 * would compose the same `onClick` twice (mergeProps stacks event
 * handlers when both `elementProps` and `toast.actionProps` carry one).
 *
 * `swipeDirection` propagates from the Provider via `ToastConfigContext`
 * so callers get a single global setting on `<Toast.Provider>` instead
 * of repeating it per toast. */
function DefaultToastList() {
  const manager = BaseToast.useToastManager();
  const { swipeDirection } = useContext(ToastConfigContext);
  return (
    <>
      {manager.toasts.map((entry) => (
        <ToastRoot
          key={entry.id}
          toast={entry}
          swipeDirection={swipeDirection}
        >
          <div className="zs-toast-content">
            {entry.title ? <ToastTitle>{entry.title}</ToastTitle> : null}
            {entry.description ? (
              <ToastDescription>{entry.description}</ToastDescription>
            ) : null}
            {entry.actionProps ? <ToastAction /> : null}
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

/**
 * Public props for `Toast.Root`.
 *
 * `role`, `aria-live`, and Base UI's polymorphic `render` are stripped
 * from the public surface — the variant-resolved role/live-region
 * mapping (status/polite for default/info/success, status/assertive
 * for warning, alert/assertive for error) is a brief-level contract;
 * callers MUST NOT be able to silently downgrade it via a stray
 * prop spread or a custom `render` that drops the attrs on the floor.
 * Internal `data-variant` / `className` reach the DOM unconditionally
 * because we apply them last, after `...rest`.
 */
export interface ToastRootProps
  extends Omit<
    BaseRootProps,
    "className" | "swipeDirection" | "role" | "aria-live" | "render"
  > {
  /**
   * Direction(s) the toast can be swiped to dismiss. Logical `start`/
   * `end` map to the inline axis (resolved at render time off the
   * `DirectionProvider` direction so RTL flips for free); `up`/`down`
   * map to the block axis. Pass an array to enable multiple directions.
   *
   * Omit on the Root to inherit the Provider-level `swipeDirection`.
   *
   * @default ["end", "down"]
   */
  swipeDirection?: ToastSwipeDirection | ToastSwipeDirection[];
  /** Optional class hook on the toast root element. */
  className?: string;
}

/** Resolve our logical `start`/`end` swipe ends to the physical
 * `left`/`right` Base UI consumes, mirrored under RTL. We read direction
 * off Base UI's `DirectionProvider` (defaults to `'ltr'` when the
 * provider is absent — see `DirectionContext` in `@base-ui/react`) so
 * `dir="rtl"` content gets gestures that match the visual layout. */
function resolveSwipeDirection(
  swipe: ToastSwipeDirection | ToastSwipeDirection[] | undefined,
  direction: "ltr" | "rtl",
): BaseRootProps["swipeDirection"] {
  const inlineStart = direction === "rtl" ? "right" : "left";
  const inlineEnd = direction === "rtl" ? "left" : "right";
  const map: Record<ToastSwipeDirection, "left" | "right" | "up" | "down"> = {
    start: inlineStart,
    end: inlineEnd,
    up: "up",
    down: "down",
  };
  if (swipe === undefined) return [inlineEnd, "down"];
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

/** Keys we strip from any `...rest` before passing through to Base UI,
 * so consumers who reach in via a type cast still can't downgrade the
 * variant-locked ARIA contract. Kept colocated with the resolvers so
 * the lock list never drifts from the props it protects.
 *
 * `aria-modal` is also locked AND force-set to `undefined` below because
 * Base UI's Toast.Root defaults `aria-modal="false"` (carried over from
 * the dialog-flavoured base), and `aria-modal` is not a valid attribute
 * on `role="status"` / `role="alert"` — axe flags it as a critical
 * `aria-allowed-attr` violation. The brief's variant contract is the
 * live-region pair, not a modal flag. */
const LOCKED_ROOT_KEYS = ["role", "aria-live", "aria-modal", "render"] as const;

const ToastRoot = forwardRef<HTMLDivElement, ToastRootProps>(
  function ToastRoot(
    { toast, swipeDirection, className, ...rest },
    ref,
  ) {
    const direction = useDirection();
    // `toast.type` is our variant — set by useToast() when adding.
    // Default to "default" when omitted so role/aria-live still resolve.
    const variant = (toast.type as ToastVariant | undefined) ?? "default";
    // Strip locked attributes from `rest` so a runtime cast (or a stale
    // descriptor on a forwarded ref) can't override them. Internal
    // values are written below, after the spread, so the variant-derived
    // contract wins regardless of caller intent.
    const restRecord = rest as Record<string, unknown>;
    for (const key of LOCKED_ROOT_KEYS) {
      delete restRecord[key];
    }
    return (
      <BaseToast.Root
        ref={ref}
        toast={toast}
        {...rest}
        swipeDirection={resolveSwipeDirection(swipeDirection, direction)}
        role={resolveRole(variant)}
        aria-live={resolveAriaLive(variant)}
        /* Base UI stamps `aria-hidden="true"` on high-priority
         * (warning/error) toasts until they receive keyboard focus —
         * see @base-ui/react/toast/root/ToastRoot.js. The intent is
         * to avoid screen-reader double-announcement (once via the
         * live region, once via DOM-tree navigation). Modern screen
         * readers (NVDA, JAWS, VoiceOver) deduplicate live-region
         * announcements on their own, so the extra aria-hidden costs
         * us testability (Testing Library treats aria-hidden=true as
         * not-visible, which makes the Root AND every child
         * untestable via `toBeVisible()`) for no real a11y gain.
         * Force it back to undefined so the live region remains the
         * sole announce channel and the rendered toast is
         * AT-discoverable on focus as it should be. */
        aria-hidden={undefined}
        // Suppress Base UI's default `aria-modal="false"` — invalid on
        // `role="status"`/`role="alert"`, and a toast is never modal
        // anyway. Setting `undefined` lets `mergeProps`'s last-wins rule
        // omit the attribute from the rendered DOM.
        aria-modal={undefined}
        data-variant={variant}
        className={classnames(
          "zs-toast-root",
          `zs-toast-root--${variant}`,
          className,
        )}
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
  function ToastClose(
    {
      className,
      children,
      "aria-label": ariaLabel,
      ...rest
    },
    ref,
  ) {
    return (
      <BaseToast.Close
        ref={ref}
        className={composeBaseClass("zs-toast-close", className)}
        {...rest}
        /* Apply `aria-label` AFTER the spread so the default survives
         * the common `aria-label={maybeLabel}` pattern when `maybeLabel`
         * is `undefined`. Destructuring `aria-label` out of `rest` above
         * prevents an explicit `undefined` from blowing the default
         * away — `?? "Dismiss notification"` is the floor.
         *
         * `aria-hidden` is force-undefined for the same reason
         * `Toast.Root` neutralizes Base UI's high-priority `aria-hidden`
         * (root/ToastRoot.js:462): Base UI parks `aria-hidden="true"`
         * on the close button while the viewport is unexpanded (see
         * close/ToastClose.js:49 `aria-hidden: !expanded && !hasFocus`)
         * to dedupe screen-reader announcements with the live region.
         * Modern screen readers already dedupe announcements; the side
         * effect is that the button is focusable inside an aria-hidden
         * subtree, which axe flags as `aria-hidden-focus` (critical).
         * Forcing `undefined` keeps the button discoverable to AT users
         * AND satisfies the axe rule. Setting `undefined` instead of
         * `false` lets `mergeProps` omit the attribute entirely. */
        aria-label={ariaLabel ?? "Dismiss notification"}
        aria-hidden={undefined}
      >
        {children ?? <Icon as={X} size="sm" />}
      </BaseToast.Close>
    );
  },
);
ToastClose.displayName = "Toast.Close";

/* ─── Public manager hook ───────────────────────────────────────────── *
 *
 * `useToastManager` exposes Base UI's low-level manager (toast list,
 * `add` / `update` / `close` / `promise`) so consumers writing a
 * custom Viewport render-prop can subscribe to the same store the
 * default loop uses. The imperative `useToast()` hook is the canonical
 * surface; this is the escape hatch for callers who explicitly opt
 * into the compound API. */
export const useToastManager = BaseToast.useToastManager;

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
