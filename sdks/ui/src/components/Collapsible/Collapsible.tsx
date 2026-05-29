/*
 * Collapsible — Root + Trigger + Panel.
 *
 * Wraps Base UI's `Collapsible` primitive. Where Accordion stacks
 * disclosure rows under a shared root, Collapsible is the single-row
 * version — one Trigger toggles one Panel. Reads as a "Show more / hide"
 * affordance on a row, a "Show advanced" toggle on a form, or a
 * lightweight inline disclosure inside a Card.
 *
 *   - Collapsible.Root: owns controlled / uncontrolled `open`, disabled
 *     state. The Root is a `<div>` so it can be styled as a card or
 *     wrap inline content.
 *
 *   - Collapsible.Trigger: a real `<button type="button">` with
 *     `aria-expanded` and `aria-controls` wired to its Panel. The
 *     `type="button"` stamp is unconditional and not overridable
 *     from the public prop surface, so a Collapsible inside a Form
 *     never submits it. Disabled triggers don't respond to clicks
 *     and read inactive. The Trigger also supports `asChild` for
 *     swapping in a custom host element (`<a>`, custom button, etc.)
 *     via the shared `_slot.ts` Slot helper — mirrors Toggle / Drawer
 *     / Dialog.Close (commit 3a64a726).
 *
 *   - Collapsible.Panel: a `<div>` carrying the matching `id`. Base
 *     UI tags it with `data-open` / `data-closed` plus the transient
 *     `data-starting-style` and `data-ending-style` attributes while
 *     the panel animates in / out. Our CSS keys block-size off those
 *     attributes — `0` when closed (and during the starting / ending
 *     transition frames), `var(--collapsible-panel-height)` when open.
 *     `prefers-reduced-motion: reduce` snaps without a transition. The
 *     Panel is `[hidden]` when fully closed so its content stays out
 *     of the tab order. The Panel also supports `asChild` so the
 *     consumer can render-as their own element (e.g., a `<section>`)
 *     while keeping Base UI's panel mechanics.
 *
 * Design guarantees encoded in source:
 *
 *   1. Trigger is ALWAYS a `<button type="button">` on the default
 *      render path. We stamp `type` unconditionally on the JSX and
 *      omit it from the public props so a consumer cannot spread
 *      `type="submit"`. Same defense Accordion / Tabs / Toggle apply
 *      — a Collapsible inside a Form must NOT submit. On the asChild
 *      path the caller's element decides its own tag; if they pass a
 *      `<button>` we drive `nativeButton={true}` so Base UI's native
 *      handlers stay, anything else flips `nativeButton={false}` and
 *      Base UI supplies the keyboard handlers + `role="button"`.
 *
 *   2. `disabled` cascades to the Trigger so the affordance reads
 *      inactive without the consumer threading the prop through twice.
 *
 *   3. Panel animates via Base UI's `--collapsible-panel-height` CSS
 *      var, paired with `prefers-reduced-motion` to flatten the
 *      animation. The chevron rotation on the Trigger also flattens in
 *      reduced motion.
 *
 *   4. Forced colors: the Trigger ink resolves to `CanvasText` /
 *      `HighlightText` (open state), focus to `Highlight`. The Panel
 *      content stays on `CanvasText` for legibility. The chevron's
 *      colour is mirrored inside the forced-colors block so the
 *      default-path chevron does not keep a token colour under
 *      Windows High Contrast. An open + disabled Trigger paints
 *      GrayText (the disabled contract beats the open contract).
 *
 *   5. RTL via logical properties. The chevron lives at `inset-inline-
 *      end` so RTL swaps it automatically.
 *
 *   6. Compound namespace `Collapsible.{Root,Trigger,Panel}` — calling
 *      `<Collapsible>` directly mounts Root.
 *
 *   7. `asChild` on Trigger and Panel routes through the canonical
 *      `_slot.ts` Slot helper (mirrors Dialog.Close, commit 3a64a726
 *      / Toggle / Drawer). When `asChild` is set on Trigger the
 *      caller's element replaces the default `<button>` AND the
 *      auto-injected chevron — the caller is responsible for rendering
 *      whatever chrome they want. On Panel, `asChild` swaps the outer
 *      `<div>` for the caller's element AND drops the
 *      `.zs-collapsible-panel-inner` padding wrapper, because the
 *      consumer-supplied element owns its own padding.
 *
 *   8. `onOpenChange` receives Base UI's full change details object
 *      as a second argument so consumers can read the change
 *      `reason`, inspect the native event, and call `details.cancel()`
 *      to veto the change before Base UI applies it.
 */
import {
  forwardRef,
  isValidElement,
  type ComponentPropsWithRef,
  type ReactElement,
  type ReactNode,
} from "react";
import { Collapsible as BaseCollapsible } from "@base-ui/react/collapsible";
import { Slot } from "../_slot";
import { classnames } from "../_classnames";

/* ─── Collapsible root ──────────────────────────────────────────────── */

type BaseCollapsibleRootProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Root
>;

/**
 * `onOpenChange` signature derived from Base UI so the second
 * `details` argument (reason, native event, `cancel()`) is preserved.
 * Wave 10 rework: narrowing to `(open: boolean) => void` swallowed
 * the cancellation hook + native event reference, which Base UI's
 * Root honors before updating uncontrolled state. Public consumers
 * MUST receive both args.
 */
type CollapsibleOnOpenChange = NonNullable<
  BaseCollapsibleRootProps["onOpenChange"]
>;

export interface CollapsibleRootProps
  extends Omit<
    BaseCollapsibleRootProps,
    | "render"
    | "className"
    | "open"
    | "defaultOpen"
    | "onOpenChange"
    | "disabled"
    | "children"
  > {
  /**
   * Controlled open state. When provided, `onOpenChange` MUST also be
   * provided to react to user-driven toggles. Omit both to use
   * `defaultOpen` for uncontrolled.
   */
  open?: boolean;
  /**
   * Uncontrolled initial open state. Pair with `onOpenChange` to
   * observe changes without taking control.
   *
   * @default false
   */
  defaultOpen?: boolean;
  /**
   * Called when the open state changes. Receives the Base UI change
   * details (reason, native event, `cancel()`) as a second argument
   * — call `details.cancel()` to veto the change before Base UI
   * applies it to uncontrolled state.
   */
  onOpenChange?: CollapsibleOnOpenChange;
  /**
   * Whether the component should ignore user interaction. Cascades to
   * the Trigger so the affordance reads inactive.
   *
   * @default false
   */
  disabled?: boolean;
  /** Optional class hook on the root container. */
  className?: string;
  /** Collapsible children — `<Collapsible.Trigger>` + `<Collapsible.Panel>`. */
  children: ReactNode;
}

const CollapsibleRoot = forwardRef<HTMLDivElement, CollapsibleRootProps>(
  function CollapsibleRoot(
    {
      open,
      defaultOpen = false,
      onOpenChange,
      disabled = false,
      className,
      children,
      ...rest
    },
    ref,
  ) {
    return (
      <BaseCollapsible.Root
        {...rest}
        ref={ref}
        open={open}
        defaultOpen={defaultOpen}
        onOpenChange={
          onOpenChange
            ? (next, details) => onOpenChange(next, details)
            : undefined
        }
        disabled={disabled}
        className={classnames("zs-collapsible", className)}
      >
        {children}
      </BaseCollapsible.Root>
    );
  },
);
(CollapsibleRoot as { displayName?: string }).displayName =
  "Collapsible.Root";

/* ─── Collapsible.Trigger ───────────────────────────────────────────── */

type BaseCollapsibleTriggerProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Trigger
>;

export interface CollapsibleTriggerProps
  extends Omit<
    BaseCollapsibleTriggerProps,
    "render" | "className" | "type" | "children"
  > {
  /** Optional class hook on the trigger button. */
  className?: string;
  /**
   * Trigger content. On the default render path this becomes the
   * label inside the auto-injected `<button>` (the chevron is added
   * at `inset-inline-end`). On the `asChild` path this MUST be a
   * single React element — the auto chevron is NOT injected, the
   * caller's element is rendered verbatim.
   */
  children?: ReactNode;
  /**
   * Render as the single child element rather than the default
   * `<button>` + chevron. Used to swap the host element to `<a>`,
   * a custom button component, etc., while keeping Base UI's
   * open / close machinery + aria wiring. Routes through the shared
   * `_slot.ts` Slot helper so className, style, refs, AND event
   * handlers compose with whatever Base UI emits (mirrors
   * `Dialog.Close 3a64a726`). Pass a single React element child —
   * anything else is a dev-error.
   *
   * @default false
   */
  asChild?: boolean;
}

const CollapsibleTrigger = forwardRef<
  HTMLButtonElement,
  CollapsibleTriggerProps
>(function CollapsibleTrigger(
  { asChild = false, className, children, ...rest },
  ref,
) {
  // Detect whether the asChild target is a native `<button>` so we
  // can drive Base UI's `nativeButton` correctly. Same heuristic
  // Toggle / Dialog.Close / Drawer.Close use.
  const asChildIsNativeButton =
    asChild &&
    isValidElement(children) &&
    (children as { type?: unknown }).type === "button";

  // Default path: a real `<button>` so `nativeButton={true}`. asChild
  // path: trust the inspection above.
  const nativeButton = asChild ? asChildIsNativeButton : true;

  if (
    process.env.NODE_ENV !== "production" &&
    asChild &&
    !isValidElement(children)
  ) {
    // eslint-disable-next-line no-console
    console.error(
      "Collapsible.Trigger asChild expects a single React element child; received " +
        typeof children +
        "; rendering nothing.",
    );
  }

  if (asChild) {
    return (
      <BaseCollapsible.Trigger
        {...rest}
        ref={ref}
        nativeButton={nativeButton}
        className={classnames("zs-collapsible-trigger", className)}
        render={(triggerProps) => {
          if (!isValidElement(children)) return <></>;
          // Slot fans className, style, refs, and event handlers from
          // `triggerProps` onto the consumer's element. Base UI's own
          // ref arrives via `triggerProps.ref`; we leave Slot to
          // compose it with the child's ref. The Trigger's forwarded
          // ref is already attached to `<BaseCollapsible.Trigger>`,
          // so we do NOT re-compose it here (Toggle review-fix item 6:
          // re-attaching the caller ref double-fires callback refs).
          return (
            <Slot {...(triggerProps as Record<string, unknown>)}>
              {children as ReactElement}
            </Slot>
          );
        }}
      />
    );
  }

  return (
    <BaseCollapsible.Trigger
      {...rest}
      ref={ref}
      // Stamp type="button" UNCONDITIONALLY on the default path.
      // `type` is omitted from CollapsibleTriggerProps so a caller
      // can't override the stamp by spreading `type="submit"` through
      // `...rest`. Mirrors the Accordion / Tabs form-safety rule.
      type="button"
      nativeButton={nativeButton}
      className={classnames("zs-collapsible-trigger", className)}
    >
      <span className="zs-collapsible-trigger-label">{children}</span>
      <CollapsibleChevron />
    </BaseCollapsible.Trigger>
  );
});
(CollapsibleTrigger as { displayName?: string }).displayName =
  "Collapsible.Trigger";

function CollapsibleChevron() {
  return (
    <svg
      className="zs-collapsible-trigger-chevron"
      viewBox="0 0 12 12"
      aria-hidden="true"
      focusable="false"
    >
      <path
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        d="M3 4.5l3 3 3-3"
      />
    </svg>
  );
}

/* ─── Collapsible.Panel ─────────────────────────────────────────────── */

type BaseCollapsiblePanelProps = ComponentPropsWithRef<
  typeof BaseCollapsible.Panel
>;

export interface CollapsiblePanelProps
  extends Omit<BaseCollapsiblePanelProps, "render" | "className" | "children"> {
  /** Optional class hook on the panel wrapper. */
  className?: string;
  /**
   * Panel content. On the default render path this is wrapped in a
   * `.zs-collapsible-panel-inner` padding container so the outer
   * `block-size` animation can cleanly drive `0`. On the `asChild`
   * path the caller's element is rendered verbatim — the inner
   * padding wrapper is NOT injected (the consumer owns padding).
   */
  children?: ReactNode;
  /**
   * Render as the single child element rather than the default
   * `<div>` + inner padding wrapper. Use to render-as a `<section>`
   * (with `aria-labelledby`), an `<article>`, or any other host
   * element while keeping Base UI's panel mechanics (height
   * measurement, `data-open` / `data-starting-style` /
   * `data-ending-style`). Routes through the shared `_slot.ts` Slot
   * helper. Pass a single React element child — anything else is a
   * dev-error.
   *
   * @default false
   */
  asChild?: boolean;
}

const CollapsiblePanel = forwardRef<HTMLDivElement, CollapsiblePanelProps>(
  function CollapsiblePanel(
    { asChild = false, className, children, ...rest },
    ref,
  ) {
    if (
      process.env.NODE_ENV !== "production" &&
      asChild &&
      !isValidElement(children)
    ) {
      // eslint-disable-next-line no-console
      console.error(
        "Collapsible.Panel asChild expects a single React element child; received " +
          typeof children +
          "; rendering nothing.",
      );
    }

    if (asChild) {
      return (
        <BaseCollapsible.Panel
          {...rest}
          ref={ref}
          className={classnames("zs-collapsible-panel", className)}
          render={(panelProps) => {
            if (!isValidElement(children)) return <></>;
            // The caller's element replaces our default `<div>` AND
            // the inner padding wrapper. Slot composes the
            // Base-UI-emitted className / style / data-* attributes
            // and ref onto the consumer's element. We deliberately
            // do NOT re-compose `ref` here — Base UI already
            // forwards our `ref` through `panelProps.ref`.
            return (
              <Slot {...(panelProps as Record<string, unknown>)}>
                {children as ReactElement}
              </Slot>
            );
          }}
        />
      );
    }

    return (
      <BaseCollapsible.Panel
        {...rest}
        ref={ref}
        className={classnames("zs-collapsible-panel", className)}
      >
        {/* Inner wrapper carries the padding so the outer block-size
            animation cleanly drives `block-size: 0`. */}
        <div className="zs-collapsible-panel-inner">{children}</div>
      </BaseCollapsible.Panel>
    );
  },
);
(CollapsiblePanel as { displayName?: string }).displayName =
  "Collapsible.Panel";

/* ─── Compose the public namespace ──────────────────────────────────── */

const ForwardedCollapsible = CollapsibleRoot as typeof CollapsibleRoot & {
  Root: typeof CollapsibleRoot;
  Trigger: typeof CollapsibleTrigger;
  Panel: typeof CollapsiblePanel;
};
ForwardedCollapsible.Root = CollapsibleRoot;
ForwardedCollapsible.Trigger = CollapsibleTrigger;
ForwardedCollapsible.Panel = CollapsiblePanel;
(ForwardedCollapsible as { displayName?: string }).displayName = "Collapsible";

export const Collapsible = ForwardedCollapsible;
export { CollapsibleRoot, CollapsibleTrigger, CollapsiblePanel };
