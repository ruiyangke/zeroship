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
 *     physical `top` / `right` / `bottom` / `left`. Base UI 1.5's
 *     Positioner consumes the logical values directly via its own
 *     `useDirection()` hook — we forward them through unchanged and let
 *     Base UI resolve against the surrounding `<DirectionProvider>`. No
 *     hand-rolled translation: an earlier wrapper read
 *     `document.documentElement.dir` (not reactive, not honoring nested
 *     DirectionProvider) and got it wrong. Brief anti-pattern guard:
 *     callers should never write raw `left` / `right` when they mean
 *     "edge nearest the inline start" — the logical sides do the right
 *     thing under RTL automatically.
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
 *   - Crystal material pattern: the Popup paints a computable
 *     `background-color`; material, rim, and shadow tokens are
 *     augmentation only, never the sole visual signal.
 */
import {
  createContext,
  forwardRef,
  isValidElement,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useSyncExternalStore,
  type ComponentPropsWithoutRef,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { PreviewCard as BasePreviewCard } from "@base-ui/react/preview-card";
import { Slot, composeRefs } from "../_slot";
import { classnames, composeBaseClass } from "../_classnames";

/**
 * Physical and logical sides the Popup can anchor on. Derived from Base
 * UI 1.5's Positioner `side` prop (`'top' | 'bottom' | 'left' | 'right'
 * | 'inline-start' | 'inline-end'`). Base UI's Positioner resolves the
 * logical values against the surrounding `<DirectionProvider>` via
 * `useDirection()` — the wrapper does NOT translate sides itself.
 *
 * `NonNullable` strips Base UI's optional-marker so consumers writing
 * `side="inline-end"` (etc.) get a closed union, not `<value> |
 * undefined`. The Positioner type is sourced via
 * `ComponentPropsWithoutRef` to avoid coupling to its deep import path.
 */
export type PreviewCardSide = NonNullable<
  ComponentPropsWithoutRef<typeof BasePreviewCard.Positioner>["side"]
>;

/** Alignment along the chosen `side`. */
export type PreviewCardAlign = "start" | "center" | "end";

/** Max-inline-size preset. `sm` 16rem · `md` 22rem · `lg` 28rem. */
export type PreviewCardSize = "sm" | "md" | "lg";

type BaseRootProps = ComponentPropsWithRef<typeof BasePreviewCard.Root>;
type BasePreviewCardHandle = ReturnType<typeof BasePreviewCard.createHandle>;

/**
 * Wrapper handle. Base UI's `createHandle` returns a `PreviewCardHandle`
 * that connects a detached Trigger to a Root. We augment it with:
 *
 *   - `popupId`: a stable id so detached Triggers can wire
 *     `aria-describedby` to the popup the same way Triggers inside a
 *     Root context do — without it, a detached Trigger would lose the
 *     wiring (Root's runtime context is the only other carrier).
 *   - `delay` / `closeDelay`: timing the paired Root publishes via the
 *     handle so detached Triggers honor the Root's `delay` / `closeDelay`
 *     props. React context cannot bridge a Root subtree to a Trigger
 *     mounted somewhere else in the tree, so the augmented handle is
 *     the only carrier. Detached Triggers read these whenever they fall
 *     back from the React-context path.
 *
 * `delay` / `closeDelay` are mutable on the handle so a Root can update
 * them across renders without forcing the consumer to recreate the
 * handle every time. The handle is created outside React; mutating its
 * fields from a Root effect is the documented way to thread timing
 * through it.
 */
export type PreviewCardHandle = BasePreviewCardHandle & {
  /** Stable id used for `aria-describedby` ↔ Popup `id` wiring. */
  readonly popupId: string;
  /**
   * Open delay (ms) published by the paired Root. `undefined` when the
   * Root did not pass `delay` — Base UI's built-in default applies.
   */
  delay: number | undefined;
  /**
   * Close delay (ms) published by the paired Root. `undefined` when the
   * Root did not pass `closeDelay` — the wrapper's 200ms brief default
   * is applied at the Trigger.
   */
  closeDelay: number | undefined;
};

/* ─── Handle subscriber registry ───────────────────────────────────── *
 *
 * Detached Triggers need to RE-RENDER when their paired Root mutates
 * the handle's `delay` / `closeDelay` so they pick up the new timing
 * and forward it to Base UI. React doesn't observe plain-object
 * mutations on its own, so we wire a tiny pub-sub keyed by handle.
 * `useSyncExternalStore` in the Trigger subscribes to its handle's
 * notifier; Root calls `notifyPreviewCardHandle` after every write.
 *
 * Module-scoped WeakMap so the subscribers don't pollute the public
 * `PreviewCardHandle` shape, and so the registry is GC-clean when a
 * handle is no longer referenced.
 */
const handleSubscribers = new WeakMap<
  PreviewCardHandle,
  Set<() => void>
>();

function subscribeToPreviewCardHandle(
  handle: PreviewCardHandle | undefined,
  listener: () => void,
): () => void {
  if (!handle) return () => {};
  let set = handleSubscribers.get(handle);
  if (!set) {
    set = new Set();
    handleSubscribers.set(handle, set);
  }
  set.add(listener);
  return () => {
    set?.delete(listener);
  };
}

function notifyPreviewCardHandle(handle: PreviewCardHandle): void {
  const set = handleSubscribers.get(handle);
  if (!set) return;
  for (const listener of set) listener();
}

/**
 * Create an imperative handle for pairing a Root with detached Triggers
 * (mirrors Tooltip / Popover / Dialog). The returned handle carries a
 * stable `popupId` so a Trigger outside the Root's React subtree still
 * publishes `aria-describedby={handle.popupId}` — the wiring Root
 * context normally provides is preserved via the handle instead.
 *
 * `delay` / `closeDelay` start undefined; the paired Root assigns them
 * during render whenever the consumer passes those props. Detached
 * Triggers reading from this handle then honor the Root's timing
 * instead of silently falling back to Base UI's defaults.
 */
export function createPreviewCardHandle(): PreviewCardHandle {
  const handle = BasePreviewCard.createHandle() as PreviewCardHandle;
  // Generate a stable id without relying on React's useId (the handle
  // is created outside of render). The id only needs to be unique per
  // handle instance; collisions across handles are harmless because
  // each instance pairs a single Root with its own Triggers.
  const id = `zs-previewcard-${Math.random().toString(36).slice(2, 10)}`;
  Object.defineProperty(handle, "popupId", {
    value: id,
    writable: false,
    enumerable: true,
    configurable: false,
  });
  // Initialise the timing fields so detached Triggers can safely read
  // them even before the paired Root has rendered. Mutable (writable:
  // true) so the Root can update them across renders.
  Object.defineProperty(handle, "delay", {
    value: undefined,
    writable: true,
    enumerable: true,
    configurable: false,
  });
  Object.defineProperty(handle, "closeDelay", {
    value: undefined,
    writable: true,
    enumerable: true,
    configurable: false,
  });
  return handle;
}

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
   * opens. When omitted, Base UI's intent timer applies (currently
   * 600ms). Pass an explicit value to override per-Root; set to `0` for
   * keyboard-focused or "quick reveal" surfaces; raise it to suppress
   * drive-by hovers on dense link clusters. Asymmetric vs `closeDelay`:
   * the wrapper deliberately does NOT force an open default since Base
   * UI's 600ms already matches the brief.
   */
  delay?: number;
  /**
   * Milliseconds after pointer-leave before the Popup closes. The grace
   * window lets the cursor cross from Trigger into the floating Popup
   * without the panel disappearing in transit. Default `200` (the
   * wrapper overrides Base UI's heavier 300ms default to match the
   * brief; pass an explicit value here to override per-Root).
   */
  closeDelay?: number;
  /**
   * Composed children — typically a `PreviewCard.Trigger` paired with a
   * `PreviewCard.Portal`-wrapped `PreviewCard.Popup`. The Root is a
   * context-only node; it does not render visible chrome itself.
   */
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
  // When a `handle` is supplied, prefer its stable popupId so detached
  // Triggers (which read popupId off the handle directly) and Triggers
  // inside this Root's React subtree (which read it off the runtime
  // context) end up referencing the SAME id. Without this, a handle-
  // paired Root would publish one id via context and the handle would
  // carry another, splitting the aria-describedby wiring.
  const handle = (rest as { handle?: PreviewCardHandle }).handle;
  const generatedId = useId();
  const popupId = handle?.popupId ?? generatedId;
  // Publish Root timing onto the augmented handle so detached Triggers
  // (paired via the same handle) honor the Root's `delay` / `closeDelay`
  // even though they live outside this Root's React subtree. Without
  // this carrier, a detached `<PreviewCard.Trigger handle={h}>` only
  // sees Base UI's defaults (600ms open / 300ms close) — the Root's
  // `delay={0}` or `closeDelay={500}` silently does nothing. We
  // mutate during render (safe — the handle is module-scoped, not
  // React state) and notify subscribers in a layout effect so any
  // detached Trigger that already rendered with stale handle values
  // picks up the change on the same commit.
  if (handle) {
    handle.delay = delay;
    handle.closeDelay = closeDelay;
  }
  useEffect(() => {
    if (handle) notifyPreviewCardHandle(handle);
  }, [handle, delay, closeDelay]);
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

    // Detached-trigger fallback: when a Trigger lives outside any
    // PreviewCard.Root subtree (`createHandle()` pairing), the Root
    // runtime context is null. The wrapper's augmented handle carries a
    // stable `popupId` so we can still wire `aria-describedby` to it —
    // without this, detached Triggers would silently lose the wiring.
    const handleFromProps =
      (rest as { handle?: PreviewCardHandle }).handle ?? undefined;
    const resolvedPopupId = rootCtx?.popupId ?? handleFromProps?.popupId;

    // Subscribe to the paired handle's notifier so this Trigger
    // re-renders whenever the Root mutates `delay` / `closeDelay`
    // across mounts. React doesn't observe plain-object mutations on
    // its own, and detached Triggers can render BEFORE the Root's
    // effect runs on first commit — without this subscription, the
    // Trigger reads `undefined` once and never re-renders to pick up
    // the Root's actual timing. The snapshot returns a stable token
    // so React only re-renders when timing actually changes.
    const handleSubscribe = useCallback(
      (listener: () => void) =>
        subscribeToPreviewCardHandle(handleFromProps, listener),
      [handleFromProps],
    );
    const handleSnapshot = useCallback(
      () =>
        handleFromProps
          ? `${handleFromProps.delay ?? "_"}|${handleFromProps.closeDelay ?? "_"}`
          : "",
      [handleFromProps],
    );
    useSyncExternalStore(
      handleSubscribe,
      handleSnapshot,
      handleSnapshot, // SSR snapshot — same as client; timing read happens lazily during render
    );

    // Compose any consumer-supplied aria-describedby with the wrapper's
    // popup id so AT announces the preview content on focus. We wire
    // this unconditionally — referring to a non-mounted id is a no-op
    // for assistive tech (the AT only dereferences describedby AFTER
    // focus + mount).
    const ariaDescribedBy =
      ariaDescribedByProp && resolvedPopupId
        ? `${ariaDescribedByProp} ${resolvedPopupId}`
        : (ariaDescribedByProp ?? resolvedPopupId);

    // Brief defaults: 600ms open, 200ms close. Forward only when Root
    // explicitly supplied a value so per-Root timing remains optional
    // (Base UI's default applies for `delay`; we override the close
    // default since Base UI's 300ms is heavier than the brief asks for).
    //
    // Detached Triggers (no Root context) read the same timing off the
    // augmented handle, which the paired Root mutates during its
    // render. Without this carrier, a `<PreviewCard delay={0}>` paired
    // via `createHandle()` would still wait 600ms before opening
    // because the Root's React context never reaches the Trigger.
    const resolvedDelay = rootCtx?.delay ?? handleFromProps?.delay;
    const resolvedCloseDelay =
      rootCtx?.closeDelay ?? handleFromProps?.closeDelay ?? 200;

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
      // shared helper merges className / style / refs / event handlers.
      // The wrapper's own `className` flows IN through the Slot so the
      // consumer's child element ends up with `wrapper-class + child-
      // class` — without this pass-through the wrapper className was
      // silently dropped in asChild mode (codex review fix 2).
      return (
        <BasePreviewCard.Trigger
          {...(resolvedDelay !== undefined ? { delay: resolvedDelay } : {})}
          closeDelay={resolvedCloseDelay}
          aria-describedby={ariaDescribedBy}
          {...rest}
          ref={ref as Ref<HTMLAnchorElement>}
          render={(triggerProps) => {
            const triggerRef = (triggerProps as { ref?: Ref<unknown> }).ref;
            const { className: slotClassName, ...slotProps } =
              triggerProps as { className?: string } & Record<string, unknown>;
            return (
              <Slot
                {...slotProps}
                className={classnames(
                  slotClassName as string | undefined,
                  className,
                )}
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

/* Side handling is delegated to Base UI 1.5's Positioner. Its `Side`
 * type is `'top' | 'bottom' | 'left' | 'right' | 'inline-start' |
 * 'inline-end'` — `useDirection()` resolves the logical values against
 * the surrounding `<DirectionProvider>`. An earlier wrapper translated
 * the logical sides itself by reading `document.documentElement.dir`,
 * which (a) ignored nested `DirectionProvider`s, (b) wasn't reactive,
 * and (c) reinvented what Base UI already does correctly. */

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
    return (
      <BasePreviewCard.Positioner
        className="zs-preview-card-positioner"
        side={side}
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
