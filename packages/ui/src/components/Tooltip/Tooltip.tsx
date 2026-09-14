/*
 * Tooltip — anchored label that opens on hover (or focus).
 *
 *   <Tooltip.Provider>             -- wrap once at app root
 *     <Tooltip>
 *       <Tooltip.Trigger>Help</Tooltip.Trigger>
 *       <Tooltip.Portal>
 *         <Tooltip.Popup>
 *           <Tooltip.Arrow />
 *           A short hint.
 *         </Tooltip.Popup>
 *       </Tooltip.Portal>
 *     </Tooltip>
 *   </Tooltip.Provider>
 *
 * Shape decisions:
 *   - Provider lives at the app root and shares timers across siblings
 *     (so once one tooltip is visible, hovering the next opens instantly
 *     until the timeout expires). Stories wrap each render in the
 *     Provider since Storybook doesn't have a global one.
 *   - `delay` is forwarded to the Root — Base UI uses the Provider's
 *     value when no Root-level delay is set. Set delay only when you
 *     need to override per-Root; prefer Provider-level configuration.
 *   - `className` is a Popup-level concern (Root has no DOM); consumers
 *     attach the class hook to `<Tooltip.Popup>` directly.
 *   - Tooltip Positioner mirrors Popover's pattern: mounted inside the
 *     Popup wrapper so the consumer never reaches for it directly.
 *   - Arrow uses the same 16×8 triangle Popover does. Base UI 1.5
 *     stamps `data-side` on the wrapper but does NOT auto-rotate it;
 *     Tooltip.css rotates the inner SVG per side.
 *   - Tooltip on touch: Base UI handles touch-vs-pointer correctly (a
 *     touch tap shows the tooltip while held); we don't override.
 *
 * Anti-patterns we explicitly avoid:
 *   - Tooltips on interactive controls that hide critical info: the
 *     popup carries a clear `data-state` so consumers can drive
 *     conditional content reveal off it.
 *   - Tooltip-as-modal: Base UI's TooltipRoot never traps focus — the
 *     popup is a pure label.
 *   - Forced-colors leak: every state selector is mirrored inside the
 *     forced-colors media at equal specificity (Slice-5/6/7 lesson).
 */
import {
  createContext,
  forwardRef,
  useContext,
  useId,
  useMemo,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { Tooltip as BaseTooltip } from "@base-ui/react/tooltip";
import { composeBaseClass } from "../_classnames";

export type TooltipSide = "top" | "right" | "bottom" | "left";
export type TooltipAlign = "start" | "center" | "end";

type BaseRootProps = ComponentPropsWithRef<typeof BaseTooltip.Root>;
type BaseTooltipHandle<Payload> = ReturnType<typeof BaseTooltip.createHandle<Payload>>;

/**
 * Wrapper handle. Base UI's `createHandle` returns a `TooltipHandle`
 * that pairs a detached Trigger with a Root. We augment it with a
 * stable `popupId` so a Trigger mounted outside the Root's React
 * subtree can still wire `aria-describedby` to the popup the same way
 * Triggers inside a Root context do — without it, a detached Trigger
 * would silently lose the wiring (Root's runtime context is the only
 * other carrier of the popup id). Mirrors the PreviewCard pattern.
 */
export type TooltipHandle<Payload = unknown> = BaseTooltipHandle<Payload> & {
  /** Stable id used for `aria-describedby` ↔ Popup `id` wiring. */
  readonly popupId: string;
};

/**
 * Create an imperative handle for pairing a Root with a detached
 * Trigger. The returned handle carries a stable `popupId` so a
 * Trigger outside the Root's React subtree still publishes
 * `aria-describedby={handle.popupId}` — the wiring Root context
 * normally provides is preserved via the handle instead.
 *
 * Mirrors `createPreviewCardHandle`. The id only needs to be unique
 * per handle instance; collisions across handles are harmless because
 * each instance pairs a single Root with its own Trigger.
 */
export function createTooltipHandle<Payload = unknown>(): TooltipHandle<Payload> {
  const handle = BaseTooltip.createHandle<Payload>() as TooltipHandle<Payload>;
  const id = `zs-tooltip-${Math.random().toString(36).slice(2, 10)}`;
  Object.defineProperty(handle, "popupId", {
    value: id,
    writable: false,
    enumerable: true,
    configurable: false,
  });
  return handle;
}

/**
 * Props for the Tooltip root. Mirrors Base UI's `Tooltip.Root` so every
 * escape hatch (`disableHoverablePopup`, `trackCursorAxis`, `actionsRef`,
 * `handle`, `triggerId`, `defaultTriggerId`) is forwarded. We add `delay`
 * as a documented top-level knob.
 */
export interface TooltipProps extends Omit<BaseRootProps, "render"> {
  /**
   * Milliseconds before showing the tooltip on hover. When unset, Base
   * UI uses `<Tooltip.Provider>`'s delay (or its built-in default).
   * Set this only to override per-Root; passing a value here also
   * disables the Provider's shared-timer short-circuit for this Root,
   * so prefer Provider-level configuration.
   */
  delay?: number;
  children?: ReactNode;
}

/* ─── Provider ──────────────────────────────────────────────────────── *
 *
 * The Provider shares the open-delay across sibling tooltips so once a
 * tooltip is visible the next one opens instantly. Wrap your app once
 * at the root; Storybook wraps each story's render in this component
 * since there's no global provider knob in storybook 8.6. */

type BaseProviderProps = ComponentPropsWithoutRef<typeof BaseTooltip.Provider>;
export type TooltipProviderProps = BaseProviderProps;

function TooltipProvider(props: TooltipProviderProps) {
  return <BaseTooltip.Provider {...props} />;
}
TooltipProvider.displayName = "Tooltip.Provider";

/* ─── Root ──────────────────────────────────────────────────────────── *
 *
 * Base UI scopes `delay` to the Trigger / Provider (not to the Root) —
 * the Root itself is a context-only node. We expose a Root-level `delay`
 * for API legibility and forward the value through an internal context
 * the Trigger consumes. When unset (the recommended path), Base UI
 * consults the surrounding `<Tooltip.Provider>` for shared timing; when
 * set on a Root, the per-Trigger value still wins (Base UI prefers
 * Trigger-local timing), and the Provider's shared-timer short-circuit
 * is bypassed for that Root. */

/* Runtime context — Root publishes the explicit delay (when set) and a
 * stable popup id; Trigger uses the id to set `aria-describedby` on
 * itself, and Popup uses the same id as its DOM `id`. Base UI 1.5 does
 * NOT auto-wire `aria-describedby` on tooltip triggers (Radix does), so
 * the wrapper owns the wiring to honor the brief's keyboard assertion.
 *
 * `delay` is intentionally `number | undefined`: when the consumer
 * omits Root's `delay`, the context publishes `undefined` and Trigger
 * forwards no `delay` prop, which lets Base UI consult the surrounding
 * `<Tooltip.Provider>` (or fall back to its built-in default). Defaulting
 * to a literal at the Root would silently override Provider timing.
 *
 * We wire describedby unconditionally — referring to a non-mounted id
 * is a no-op for assistive tech (the AT just doesn't find the target),
 * which is fine because the only time AT will dereference describedby
 * is after the trigger receives focus AND the popup has mounted (the
 * delay between focus and mount is well under AT polling). */
type TooltipRootRuntimeContext = {
  delay: number | undefined;
  popupId: string;
};
const TooltipRootRuntimeCtx =
  createContext<TooltipRootRuntimeContext | null>(null);

function TooltipRoot({ delay, children, ...rest }: TooltipProps) {
  // When a `handle` is supplied, prefer its stable popupId so a
  // detached Trigger (which reads popupId off the handle directly)
  // and a Trigger inside this Root's React subtree (which reads it
  // off the runtime context) end up referencing the SAME id. Without
  // this, a handle-paired Root would publish one id via context and
  // the handle would carry another, splitting the aria-describedby
  // wiring. Mirrors the PreviewCard wrapper.
  const handle = (rest as { handle?: TooltipHandle }).handle;
  const generatedId = useId();
  const popupId = handle?.popupId ?? generatedId;
  const ctxValue = useMemo<TooltipRootRuntimeContext>(
    () => ({ delay, popupId }),
    [delay, popupId],
  );
  return (
    <TooltipRootRuntimeCtx.Provider value={ctxValue}>
      <BaseTooltip.Root {...rest}>{children}</BaseTooltip.Root>
    </TooltipRootRuntimeCtx.Provider>
  );
}
TooltipRoot.displayName = "Tooltip";

/* ─── Trigger ───────────────────────────────────────────────────────── *
 *
 * Base UI's TooltipTrigger renders as a Slot-style passthrough — the
 * consumer's child element gets the hover / focus handlers + the
 * `aria-describedby` link to the popup. */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BaseTooltip.Trigger>;
export type TooltipTriggerProps = BaseTriggerProps;

const TooltipTrigger = forwardRef<HTMLElement, TooltipTriggerProps>(
  function TooltipTrigger(
    {
      delay: delayProp,
      "aria-describedby": ariaDescribedByProp,
      ...rest
    },
    ref,
  ) {
    // Pull the Root-level delay (if any) into the Trigger's `delay` so
    // the Tooltip's top-level `delay` prop, when set, drives timing.
    // When neither prop nor Root supplies `delay`, we omit it entirely
    // so Base UI falls back to `<Tooltip.Provider>`'s shared delay (or
    // its built-in default). Forwarding `delay={undefined}` would still
    // be safe today, but the explicit omission documents the contract.
    const rootCtx = useContext(TooltipRootRuntimeCtx);
    const resolvedDelay = delayProp ?? rootCtx?.delay;
    // Detached-trigger fallback: when a Trigger lives outside any
    // Tooltip Root subtree (`createTooltipHandle()` pairing), the Root
    // runtime context is null. The wrapper's augmented handle carries
    // a stable `popupId` so we can still wire `aria-describedby` to it
    // — without this, detached Triggers would silently lose the
    // wiring (the 🔴 review-3 regression). Mirrors PreviewCard.
    const handleFromProps =
      (rest as { handle?: TooltipHandle }).handle ?? undefined;
    const resolvedPopupId = rootCtx?.popupId ?? handleFromProps?.popupId;
    // Compose any consumer-supplied aria-describedby with our internal
    // tooltip-popup id so screen readers announce the tooltip when the
    // trigger receives focus.
    const ariaDescribedBy =
      ariaDescribedByProp && resolvedPopupId
        ? `${ariaDescribedByProp} ${resolvedPopupId}`
        : (ariaDescribedByProp ?? resolvedPopupId);
    return (
      <BaseTooltip.Trigger
        ref={ref as Ref<HTMLButtonElement>}
        {...(resolvedDelay !== undefined ? { delay: resolvedDelay } : {})}
        aria-describedby={ariaDescribedBy}
        {...rest}
      />
    );
  },
);
TooltipTrigger.displayName = "Tooltip.Trigger";

/* ─── Portal ────────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BaseTooltip.Portal>;
export type TooltipPortalProps = BasePortalProps;

function TooltipPortal(props: TooltipPortalProps) {
  return <BaseTooltip.Portal {...props} />;
}
TooltipPortal.displayName = "Tooltip.Portal";

/* ─── Popup ─────────────────────────────────────────────────────────── *
 *
 * Mounts inside an internal `<BaseTooltip.Positioner>` so anchoring
 * knobs (`side`, `align`, `sideOffset`) live on the Popup rather than
 * on a separate Positioner subpart the consumer would have to remember.
 * Brief default side: `top` (matches Base UI). */

type BasePopupProps = ComponentPropsWithoutRef<typeof BaseTooltip.Popup>;
/* `id` is reserved by the wrapper: Root owns a stable popup id that the
 * Trigger references via `aria-describedby`. Letting the consumer
 * override `id` would leave the Trigger pointing at a missing target
 * (the wired-up `aria-describedby` still uses Root's id) — the shared
 * id is an internal contract, not a styling hook. */
export interface TooltipPopupProps extends Omit<BasePopupProps, "id"> {
  /** Which side of the trigger to anchor on. Default `top`. */
  side?: TooltipSide;
  /** Alignment along the chosen side. Default `center`. */
  align?: TooltipAlign;
  /** Pixel offset between trigger and popup. Default `8`. */
  sideOffset?: number;
}

const TooltipPopup = forwardRef<HTMLElement, TooltipPopupProps>(
  function TooltipPopup(
    {
      side = "top",
      align = "center",
      sideOffset = 8,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    // The popup's DOM id is the shared one published by Root so the
    // Trigger's `aria-describedby` resolves here. Consumers cannot
    // override it (see `TooltipPopupProps`).
    const rootCtx = useContext(TooltipRootRuntimeCtx);
    const popupId = rootCtx?.popupId;
    return (
      <BaseTooltip.Positioner
        className="zs-tooltip-positioner"
        side={side}
        align={align}
        sideOffset={sideOffset}
      >
        <BaseTooltip.Popup
          {...rest}
          ref={ref as Ref<HTMLDivElement>}
          id={popupId}
          className={composeBaseClass("zs-tooltip-popup", className)}
        >
          {children}
        </BaseTooltip.Popup>
      </BaseTooltip.Positioner>
    );
  },
);
TooltipPopup.displayName = "Tooltip.Popup";

/* ─── Arrow ─────────────────────────────────────────────────────────── *
 *
 * Same 16×8 downward-pointing triangle Popover uses; Tooltip.css rotates
 * the inner SVG per `data-side` (Base UI 1.5 doesn't auto-rotate the
 * wrapper). Renders the default glyph but accepts custom children for
 * consumers who want a different shape. */

type BaseArrowProps = ComponentPropsWithoutRef<typeof BaseTooltip.Arrow>;
export type TooltipArrowProps = BaseArrowProps;

const TooltipArrow = forwardRef<HTMLDivElement, TooltipArrowProps>(
  function TooltipArrow({ className, children, ...rest }, ref) {
    return (
      <BaseTooltip.Arrow
        ref={ref}
        className={composeBaseClass("zs-tooltip-arrow", className)}
        {...rest}
      >
        {children ?? <ArrowGlyph />}
      </BaseTooltip.Arrow>
    );
  },
);
TooltipArrow.displayName = "Tooltip.Arrow";

function ArrowGlyph() {
  return (
    <svg
      width="16"
      height="8"
      viewBox="0 0 16 8"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M 0,0 L 8,8 L 16,0 Z" fill="currentColor" />
    </svg>
  );
}

/* ─── public namespace ──────────────────────────────────────────────── */

export type TooltipComponent = typeof TooltipRoot & {
  Provider: typeof TooltipProvider;
  Trigger: typeof TooltipTrigger;
  Portal: typeof TooltipPortal;
  Popup: typeof TooltipPopup;
  Arrow: typeof TooltipArrow;
  createHandle: typeof createTooltipHandle;
};

export const Tooltip = TooltipRoot as TooltipComponent;
Tooltip.Provider = TooltipProvider;
Tooltip.Trigger = TooltipTrigger;
Tooltip.Portal = TooltipPortal;
Tooltip.Popup = TooltipPopup;
Tooltip.Arrow = TooltipArrow;
Tooltip.createHandle = createTooltipHandle;
