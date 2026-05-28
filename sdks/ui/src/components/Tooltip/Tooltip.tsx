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
 *     value when no Root-level delay is set. Brief default is 600ms.
 *   - Tooltip Positioner mirrors Popover's pattern: mounted inside the
 *     Popup wrapper so the consumer never reaches for it directly.
 *   - Arrow uses the same 16×8 triangle Popover does (Base UI rotates
 *     the wrapping div per side).
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

/**
 * Re-export Base UI's `createHandle` so consumers can imperatively pair
 * a Trigger to a Root.
 */
export const createTooltipHandle = BaseTooltip.createHandle;

/**
 * Props for the Tooltip root. Mirrors Base UI's `Tooltip.Root` so every
 * escape hatch (`disableHoverablePopup`, `trackCursorAxis`, `actionsRef`,
 * `handle`, `triggerId`, `defaultTriggerId`) is forwarded. We add `delay`
 * + `className` as documented top-level knobs.
 */
export interface TooltipProps extends Omit<BaseRootProps, "render"> {
  /**
   * Milliseconds before showing the tooltip on hover. Default `600`.
   * When wrapped in `<Tooltip.Provider>`, an open sibling within the
   * Provider's `timeout` window short-circuits this delay (shared
   * timers are the whole point of Provider).
   */
  delay?: number;
  /** Class hook forwarded to the Popup. */
  className?: string;
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
 * the Trigger consumes. Default is 600ms (brief value); when a Provider
 * with its own delay wraps multiple tooltips, the Trigger's per-root
 * value still wins (Base UI prefers Trigger-local timing). */

/* Runtime context — Root publishes the resolved delay and a stable
 * popup id; Trigger uses the id to set `aria-describedby` on itself,
 * and Popup uses the same id as its DOM `id`. Base UI 1.5 does NOT
 * auto-wire `aria-describedby` on tooltip triggers (Radix does), so
 * the wrapper owns the wiring to honor the brief's keyboard assertion.
 *
 * We wire describedby unconditionally — referring to a non-mounted id
 * is a no-op for assistive tech (the AT just doesn't find the target),
 * which is fine because the only time AT will dereference describedby
 * is after the trigger receives focus AND the popup has mounted (the
 * delay between focus and mount is well under AT polling). */
type TooltipRootRuntimeContext = {
  delay: number;
  popupId: string;
};
const TooltipRootRuntimeCtx =
  createContext<TooltipRootRuntimeContext | null>(null);

function TooltipRoot({ delay = 600, children, ...rest }: TooltipProps) {
  const popupId = useId();
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
    // Pull the Root-level delay into the Trigger's `delay` so the API
    // surface — Tooltip's `delay` prop — actually drives the open
    // timing. Caller-supplied per-Trigger delay still wins.
    const rootCtx = useContext(TooltipRootRuntimeCtx);
    const delay = delayProp ?? rootCtx?.delay;
    // Compose any consumer-supplied aria-describedby with our internal
    // tooltip-popup id so screen readers announce the tooltip when the
    // trigger receives focus.
    const ariaDescribedBy =
      ariaDescribedByProp && rootCtx?.popupId
        ? `${ariaDescribedByProp} ${rootCtx.popupId}`
        : (ariaDescribedByProp ?? rootCtx?.popupId);
    return (
      <BaseTooltip.Trigger
        ref={ref as Ref<HTMLButtonElement>}
        delay={delay}
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
export interface TooltipPopupProps extends BasePopupProps {
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
      id: idProp,
      children,
      ...rest
    },
    ref,
  ) {
    // Pull the Root's popupId so the trigger's aria-describedby
    // resolves to this DOM node. Caller-supplied `id` wins (rare —
    // mostly story-test escape hatch).
    const rootCtx = useContext(TooltipRootRuntimeCtx);
    const popupId = idProp ?? rootCtx?.popupId;
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
 * Same 16×8 triangle Popover uses; the wrapping div rotates per side.
 * Renders the SVG triangle by default but accepts custom children for
 * consumers who want a different glyph. */

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
