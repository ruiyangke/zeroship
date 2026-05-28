/*
 * PreviewCard — hover-anchored rich preview surface.
 *
 *   <PreviewCard>
 *     <PreviewCard.Trigger>@handle</PreviewCard.Trigger>
 *     <PreviewCard.Portal>
 *       <PreviewCard.Backdrop />                 -- optional, default skipped
 *       <PreviewCard.Popup>
 *         <PreviewCard.Arrow />                  -- optional, points at trigger
 *         <img src="…" alt="…" />                -- compose freely
 *         <h3>Headline</h3>
 *         <p>Body copy.</p>
 *       </PreviewCard.Popup>
 *     </PreviewCard.Portal>
 *   </PreviewCard>
 *
 * Shape decisions:
 *   - Reads as a richer Tooltip / lighter Popover: opens on hover (with a
 *     deliberate ~600ms intent delay) AND on keyboard focus; dismisses on
 *     pointer leave with a short grace window so the user can move the
 *     cursor INTO the popup without it disappearing.
 *   - Base UI 1.5 ships `PreviewCard.*` natively (root, trigger, portal,
 *     backdrop, positioner, popup, arrow). The wrapper composes Positioner
 *     INSIDE Popup so anchoring knobs (`side`, `align`, `sideOffset`,
 *     `size`) live on a single subpart — mirrors Popover and Tooltip.
 *   - `delay` and `closeDelay` are defined on the BASE Trigger (not Root).
 *     We expose them on Root as the documented surface and forward through
 *     an internal context the Trigger consumes — same pattern Tooltip uses
 *     for its Root-level `delay`. Brief default: `delay = 600`,
 *     `closeDelay = 200`.
 *   - `side` accepts logical `inline-start` / `inline-end` IN ADDITION to
 *     physical `top` / `right` / `bottom` / `left`. The logical values are
 *     resolved to Base UI's physical sides via the surrounding
 *     `<DirectionProvider>` (Base UI flips `start` / `end` alignment for
 *     RTL automatically; we extend that to `side` too). Brief anti-pattern
 *     guard: callers should never write raw `left` / `right` when they
 *     mean "edge nearest the inline start" — the wrapper's logical sides
 *     do the right thing under RTL.
 *   - `size` resolves to a `max-inline-size` token on the popup via a
 *     `data-size` attribute (`sm` = 16rem, `md` = 22rem, `lg` = 28rem).
 *     The default size is `md`. Brief anti-pattern: there is NO `image`
 *     prop — consumers render `<img>` inside the Popup body.
 *   - The Trigger swaps between the default Base UI `<a>` element and a
 *     consumer-supplied child element via `asChild`. Mirrors Dialog.Close
 *     (commit 3a64a726) — routed through the shared `_slot.ts` Slot so
 *     className / style / event handlers / refs all compose.
 *   - `aria-describedby` on the Trigger references the Popup id so screen
 *     readers announce the preview when the trigger receives focus, even
 *     before the popup mounts. Base UI 1.5 does NOT auto-wire this for
 *     PreviewCard (it does for Tooltip but not for PreviewCard's anchor
 *     role), so the wrapper owns the wiring.
 *
 * Anti-patterns we explicitly avoid:
 *   - No `content` string prop on Root — children compose freely.
 *   - No `image` prop — render `<img>` inside Popup body.
 *   - No raw `left` / `right` placement — use `inline-start` /
 *     `inline-end` so RTL flips physically.
 *   - No `disabled` on Root — guard via consumer logic (skip mounting,
 *     gate on a state). Base UI's Root has no `disabled` knob either.
 *   - Glass-surface invariant: the Popup paints an opaque
 *     `background-color` + optional `backdrop-filter`. `backdrop-filter`
 *     is augmentation only — never the sole visual signal.
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useContext,
  useId,
  useMemo,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { PreviewCard as BasePreviewCard } from "@base-ui/react/preview-card";
import { Slot, composeRefs } from "../_slot";
import { composeBaseClass } from "../_classnames";

/**
 * Physical and logical sides the Popup can anchor on. The logical sides
 * (`inline-start` / `inline-end`) are flipped to their physical
 * counterparts based on `<DirectionProvider>` direction; LTR resolves
 * `inline-start` → `left` and `inline-end` → `right`, RTL reverses.
 */
export type PreviewCardSide =
  | "top"
  | "right"
  | "bottom"
  | "left"
  | "inline-start"
  | "inline-end";

/** Alignment along the chosen `side`. */
export type PreviewCardAlign = "start" | "center" | "end";

/** Max-inline-size preset. `sm` 16rem · `md` 22rem · `lg` 28rem. */
export type PreviewCardSize = "sm" | "md" | "lg";

type BaseRootProps = ComponentPropsWithRef<typeof BasePreviewCard.Root>;

/**
 * Re-export Base UI's `createHandle` so consumers can imperatively pair
 * a Trigger to a Root (mirrors Tooltip / Popover / Dialog).
 */
export const createPreviewCardHandle = BasePreviewCard.createHandle;

/**
 * Props for the PreviewCard root.
 *
 * `delay` and `closeDelay` live on Base UI's Trigger by API design (timing
 * is per-anchor). We expose them at Root for legibility — Tooltip uses
 * the same trick — and forward via an internal context the Trigger reads.
 */
export interface PreviewCardProps
  extends Omit<BaseRootProps, "render"> {
  /**
   * Milliseconds the pointer must rest on the Trigger before the Popup
   * opens. Default `600` (matches Base UI). Set to `0` for keyboard-
   * focused or "quick reveal" surfaces; raise it to suppress drive-by
   * hovers on dense link clusters.
   */
  delay?: number;
  /**
   * Milliseconds after pointer-leave before the Popup closes. The grace
   * window lets the cursor cross from Trigger into the floating Popup
   * without the panel disappearing in transit. Default `200`.
   */
  closeDelay?: number;
  children?: ReactNode;
}

/* ─── Root context ─────────────────────────────────────────────────── *
 *
 * Publishes the Trigger's `delay` / `closeDelay` and a stable Popup id.
 * Trigger uses the id to set `aria-describedby` on itself; Popup uses
 * the same id as its DOM `id` attribute.
 *
 * `delay` / `closeDelay` are `number | undefined`: when the consumer
 * omits them, the context publishes `undefined` and Trigger forwards
 * no prop, which lets Base UI fall back to its built-in defaults (600 /
 * 300 respectively — we override the close default to 200 via the
 * Trigger prop when Root doesn't supply one, matching the brief).
 */
type PreviewCardRootRuntimeContext = {
  delay: number | undefined;
  closeDelay: number | undefined;
  popupId: string;
};
const PreviewCardRootRuntimeCtx =
  createContext<PreviewCardRootRuntimeContext | null>(null);

function PreviewCardRoot({
  delay,
  closeDelay,
  children,
  ...rest
}: PreviewCardProps) {
  const popupId = useId();
  const ctxValue = useMemo<PreviewCardRootRuntimeContext>(
    () => ({ delay, closeDelay, popupId }),
    [delay, closeDelay, popupId],
  );
  return (
    <PreviewCardRootRuntimeCtx.Provider value={ctxValue}>
      <BasePreviewCard.Root {...rest}>{children}</BasePreviewCard.Root>
    </PreviewCardRootRuntimeCtx.Provider>
  );
}
PreviewCardRoot.displayName = "PreviewCard";

/* ─── Trigger ──────────────────────────────────────────────────────── *
 *
 * Base UI's PreviewCardTrigger renders an `<a>` by default — the
 * canonical use case is a rich preview of the link the trigger points
 * at. We expose `asChild` so callers can swap in their own anchor /
 * button / span via the shared Slot helper (no rerender-prop ceremony
 * for the common case). Mirrors Dialog.Close's `3a64a726` pattern. */

type BaseTriggerProps = ComponentPropsWithoutRef<typeof BasePreviewCard.Trigger>;

/**
 * Props for the PreviewCard trigger.
 *
 * `delay` and `closeDelay` are intentionally omitted from the public
 * surface — Root owns them. Passing them per-Trigger would defeat the
 * "one knob per Root" contract; consumers who genuinely need per-Trigger
 * timing can drop down to `<BasePreviewCard.Trigger delay={…}>` directly.
 */
export interface PreviewCardTriggerProps
  extends Omit<BaseTriggerProps, "delay" | "closeDelay"> {
  /**
   * Render as the single child element rather than the default `<a>`.
   * Routed through the shared `_slot.ts` Slot so className, style, refs,
   * AND event handlers compose. Pass a single React element child —
   * anything else logs a dev error and renders nothing.
   */
  asChild?: boolean;
  /** Optional class hook on the rendered trigger element. */
  className?: string;
  /** Trigger content. Under `asChild`, must be a single React element. */
  children?: ReactNode;
}

const PreviewCardTrigger = forwardRef<HTMLElement, PreviewCardTriggerProps>(
  function PreviewCardTrigger(
    {
      asChild = false,
      className,
      children,
      "aria-describedby": ariaDescribedByProp,
      ...rest
    },
    ref,
  ) {
    const rootCtx = useContext(PreviewCardRootRuntimeCtx);

    // Compose any consumer-supplied aria-describedby with the wrapper's
    // popup id so AT announces the preview content on focus. We wire
    // this unconditionally — referring to a non-mounted id is a no-op
    // for assistive tech (the AT only dereferences describedby AFTER
    // focus + mount).
    const ariaDescribedBy =
      ariaDescribedByProp && rootCtx?.popupId
        ? `${ariaDescribedByProp} ${rootCtx.popupId}`
        : (ariaDescribedByProp ?? rootCtx?.popupId);

    // Brief defaults: 600ms open, 200ms close. Forward only when the
    // consumer or Root explicitly supplied a value so per-Root timing
    // remains optional (Base UI's defaults otherwise apply for `delay`;
    // we override the close default since Base UI's 300ms is heavier
    // than the brief asks for).
    const resolvedDelay = rootCtx?.delay;
    const resolvedCloseDelay = rootCtx?.closeDelay ?? 200;

    if (
      process.env.NODE_ENV !== "production" &&
      asChild &&
      !isValidElement(children)
    ) {
      // eslint-disable-next-line no-console
      console.error(
        "PreviewCard.Trigger asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    if (asChild) {
      if (!isValidElement(children)) return null;
      // Slot composition matches Dialog.Close (commit 3a64a726): the
      // shared helper merges className / style / refs / event handlers
      // — we just hand Base UI a render-prop that emits a Slot.
      return (
        <BasePreviewCard.Trigger
          {...(resolvedDelay !== undefined ? { delay: resolvedDelay } : {})}
          closeDelay={resolvedCloseDelay}
          aria-describedby={ariaDescribedBy}
          {...rest}
          ref={ref as Ref<HTMLAnchorElement>}
          render={(triggerProps) => {
            const triggerRef = (triggerProps as { ref?: Ref<unknown> }).ref;
            return (
              <Slot
                {...triggerProps}
                ref={composeRefs(ref as Ref<unknown>, triggerRef)}
              >
                {children}
              </Slot>
            );
          }}
        />
      );
    }

    // Default path: Base UI renders an `<a>`. The wrapper class hook is
    // composed onto the rendered element so consumers can attach hover
    // styling at the trigger layer when needed.
    return (
      <BasePreviewCard.Trigger
        ref={ref as Ref<HTMLAnchorElement>}
        {...(resolvedDelay !== undefined ? { delay: resolvedDelay } : {})}
        closeDelay={resolvedCloseDelay}
        aria-describedby={ariaDescribedBy}
        className={composeBaseClass("zs-preview-card-trigger", className)}
        {...rest}
      >
        {children}
      </BasePreviewCard.Trigger>
    );
  },
);
PreviewCardTrigger.displayName = "PreviewCard.Trigger";

/* ─── Portal ───────────────────────────────────────────────────────── */

type BasePortalProps = ComponentPropsWithoutRef<typeof BasePreviewCard.Portal>;
export type PreviewCardPortalProps = BasePortalProps;

function PreviewCardPortal(props: PreviewCardPortalProps) {
  return <BasePreviewCard.Portal {...props} />;
}
PreviewCardPortal.displayName = "PreviewCard.Portal";

/* ─── Backdrop (opt-in) ────────────────────────────────────────────── *
 *
 * Mirrors Popover.Backdrop — present in the namespace but never auto-
 * mounted. Preview cards are anchored hover panels (link previews,
 * user-handle popovers); they rarely dim the page. Ship the subpart for
 * the modal-feel edge case (campaign teaser preview, e.g.) but keep it
 * opt-in. */

type BaseBackdropProps = ComponentPropsWithoutRef<typeof BasePreviewCard.Backdrop>;
/** Props for the optional `<PreviewCard.Backdrop>`. */
export type PreviewCardBackdropProps = BaseBackdropProps;

const PreviewCardBackdrop = forwardRef<HTMLDivElement, PreviewCardBackdropProps>(
  function PreviewCardBackdrop({ className, ...rest }, ref) {
    return (
      <BasePreviewCard.Backdrop
        ref={ref as Ref<HTMLDivElement>}
        className={composeBaseClass("zs-preview-card-backdrop", className)}
        {...rest}
      />
    );
  },
);
PreviewCardBackdrop.displayName = "PreviewCard.Backdrop";

/* ─── Positioner (exposed for symmetry; rarely reached for) ─────────── */

type BasePositionerProps = ComponentPropsWithoutRef<
  typeof BasePreviewCard.Positioner
>;
/**
 * Props for the standalone Positioner. The wrapper mounts an internal
 * Positioner inside Popup so anchoring lives on the Popup; this subpart
 * is exposed for parity with Base UI but most callers will not reach for
 * it directly.
 */
export type PreviewCardPositionerProps = BasePositionerProps;

const PreviewCardPositioner = forwardRef<
  HTMLDivElement,
  PreviewCardPositionerProps
>(function PreviewCardPositioner({ className, ...rest }, ref) {
  return (
    <BasePreviewCard.Positioner
      ref={ref as Ref<HTMLDivElement>}
      className={composeBaseClass("zs-preview-card-positioner", className)}
      {...rest}
    />
  );
});
PreviewCardPositioner.displayName = "PreviewCard.Positioner";

/* ─── Popup ────────────────────────────────────────────────────────── *
 *
 * Mounts inside an internal `<BasePreviewCard.Positioner>` so the
 * Floating UI anchoring (`side`, `align`, `sideOffset`) is configurable
 * via Popup-level props — matches Popover and Tooltip. `size` resolves
 * to `data-size` on the popup which Css interprets as a max-inline-size
 * preset. */

type BasePopupProps = ComponentPropsWithoutRef<typeof BasePreviewCard.Popup>;
/**
 * Props for the PreviewCard popup. `id` is reserved by the wrapper — the
 * stable popup id is published from Root and used both as Popup's DOM id
 * AND as the trigger's `aria-describedby`. Letting consumers override
 * would silently break the aria-wiring.
 */
export interface PreviewCardPopupProps extends Omit<BasePopupProps, "id"> {
  /**
   * Side of the trigger to anchor on. `inline-start` / `inline-end`
   * flip to their physical counterparts under `<DirectionProvider>`.
   * Default `bottom`.
   */
  side?: PreviewCardSide;
  /** Alignment along the chosen `side`. Default `center`. */
  align?: PreviewCardAlign;
  /** Pixel offset between trigger and popup. Default `8`. */
  sideOffset?: number;
  /**
   * Max-inline-size preset: `sm` 16rem · `md` 22rem · `lg` 28rem.
   * Default `md`. Forwarded to a `data-size` attribute on the popup
   * which the CSS resolves to a token-driven max-inline-size.
   */
  size?: PreviewCardSize;
}

/* Logical → physical side resolution. Base UI's Positioner takes only
 * physical sides; we translate `inline-start` / `inline-end` at the
 * Popup boundary so the public API can stay token-style without
 * leaking the resolution detail. Direction is read from the document at
 * the time the Popup mounts — the same heuristic Base UI uses for align
 * flipping (it consults the surrounding `<DirectionProvider>`). */
function resolveSide(
  side: PreviewCardSide,
): "top" | "right" | "bottom" | "left" {
  if (side === "inline-start" || side === "inline-end") {
    if (
      typeof document !== "undefined" &&
      document.documentElement.getAttribute("dir") === "rtl"
    ) {
      return side === "inline-start" ? "right" : "left";
    }
    return side === "inline-start" ? "left" : "right";
  }
  return side;
}

const PreviewCardPopup = forwardRef<HTMLDivElement, PreviewCardPopupProps>(
  function PreviewCardPopup(
    {
      side = "bottom",
      align = "center",
      sideOffset = 8,
      size = "md",
      className,
      children,
      ...rest
    },
    ref,
  ) {
    const rootCtx = useContext(PreviewCardRootRuntimeCtx);
    const popupId = rootCtx?.popupId;
    const physicalSide = resolveSide(side);
    return (
      <BasePreviewCard.Positioner
        className="zs-preview-card-positioner"
        side={physicalSide}
        align={align}
        sideOffset={sideOffset}
      >
        <BasePreviewCard.Popup
          {...rest}
          ref={ref as Ref<HTMLDivElement>}
          id={popupId}
          data-size={size}
          className={composeBaseClass("zs-preview-card-popup", className)}
        >
          {children}
        </BasePreviewCard.Popup>
      </BasePreviewCard.Positioner>
    );
  },
);
PreviewCardPopup.displayName = "PreviewCard.Popup";

/* ─── Arrow ────────────────────────────────────────────────────────── *
 *
 * Base UI 1.5 positions the arrow wrapper and stamps `data-side` on it
 * but does NOT rotate the wrapper itself. PreviewCard.css rotates the
 * inner SVG per side so the apex always points at the trigger — same
 * 16×8 triangle Popover / Tooltip use, sized via tokens. */

type BaseArrowProps = ComponentPropsWithoutRef<typeof BasePreviewCard.Arrow>;
/** Props for the optional `<PreviewCard.Arrow>`. */
export type PreviewCardArrowProps = BaseArrowProps;

const PreviewCardArrow = forwardRef<HTMLDivElement, PreviewCardArrowProps>(
  function PreviewCardArrow({ className, children, ...rest }, ref) {
    return (
      <BasePreviewCard.Arrow
        ref={ref}
        className={composeBaseClass("zs-preview-card-arrow", className)}
        {...rest}
      >
        {children ?? <ArrowGlyph />}
      </BasePreviewCard.Arrow>
    );
  },
);
PreviewCardArrow.displayName = "PreviewCard.Arrow";

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

/* ─── public namespace ─────────────────────────────────────────────── */

export type PreviewCardComponent = typeof PreviewCardRoot & {
  Trigger: typeof PreviewCardTrigger;
  Portal: typeof PreviewCardPortal;
  Backdrop: typeof PreviewCardBackdrop;
  Positioner: typeof PreviewCardPositioner;
  Popup: typeof PreviewCardPopup;
  Arrow: typeof PreviewCardArrow;
  createHandle: typeof createPreviewCardHandle;
};

export const PreviewCard = PreviewCardRoot as PreviewCardComponent;
PreviewCard.Trigger = PreviewCardTrigger;
PreviewCard.Portal = PreviewCardPortal;
PreviewCard.Backdrop = PreviewCardBackdrop;
PreviewCard.Positioner = PreviewCardPositioner;
PreviewCard.Popup = PreviewCardPopup;
PreviewCard.Arrow = PreviewCardArrow;
PreviewCard.createHandle = createPreviewCardHandle;
